use std::fmt::Debug;
use std::sync::Arc;

use actix_web::{
    error::ResponseError, get, http, middleware, web, App, HttpResponse, HttpServer, Responder,
    Scope,
};
use color_eyre::eyre::{eyre, Context};
use derive_more::Display;
use iroha_client::client::ClientQueryError as IrohaClientQueryError;
use iroha_core::smartcontracts::isi::query::Error as IrohaQueryError;
use serde::Serialize;
use std::{fmt, str::FromStr};

use crate::iroha_client_wrap::IrohaClientWrap;
use pagination::{Paginated, PaginationQueryParams};

/// Web app state that may be injected in runtime
pub struct AppData {
    /// Pre-initialized Iroha Client
    iroha_client: IrohaClientWrap,
}

impl AppData {
    /// Creates new state with provided client
    pub fn new(client: IrohaClientWrap) -> Self {
        Self {
            iroha_client: client,
        }
    }
}

/// General error for all endpoints
#[derive(Display, Debug)]
enum WebError {
    /// Some error that should be logged, but shouldn't be returned to
    /// a client. Server should return an empty 500 error instead.
    Internal(color_eyre::Report),
    /// Some resource was not found.
    NotFound,
    BadRequest(String),
}

impl WebError {
    /// Constructs from [`IrohaClientQueryError`] to [`WebError::NotFound`], if there is a [`IrohaFindError`].
    /// Otherwise, constructs [`WebError::Internal`].
    fn expect_iroha_find_error(client_error: IrohaClientQueryError) -> Self {
        match client_error {
            IrohaClientQueryError::QueryError(IrohaQueryError::Find(_err)) => Self::NotFound,
            IrohaClientQueryError::QueryError(other) => {
                Self::Internal(eyre!("FindError expected, got: {other}"))
            }
            IrohaClientQueryError::Other(other) => {
                Self::Internal(other.wrap_err("Unexpected query error"))
            }
        }
    }

    /// Constructs [`WebError::Internal`] from [`IrohaClientQueryError`].
    fn expect_iroha_any_error(client_error: IrohaClientQueryError) -> Self {
        match client_error {
            IrohaClientQueryError::QueryError(any) => {
                Self::Internal(eyre!("Iroha query error: {any}"))
            }
            IrohaClientQueryError::Other(other) => {
                Self::Internal(other.wrap_err("Unexpected query error"))
            }
        }
    }
}

impl ResponseError for WebError {
    fn error_response(&self) -> HttpResponse {
        HttpResponse::build(self.status_code())
            .insert_header(http::header::ContentType::html())
            .body(match self {
                // We don't want to expose internal errors to the client, so here it is omitted.
                // `actix-web` will log it anyway.
                Self::Internal(_) => "Internal Server Error".to_owned(),
                Self::NotFound => "Not Found".to_owned(),
                Self::BadRequest(msg) => format!("Bad Request: {}", msg),
            })
    }

    fn status_code(&self) -> http::StatusCode {
        match self {
            Self::Internal(_) => http::StatusCode::INTERNAL_SERVER_ERROR,
            Self::NotFound => http::StatusCode::NOT_FOUND,
            Self::BadRequest(_) => http::StatusCode::BAD_REQUEST,
        }
    }
}

impl From<color_eyre::Report> for WebError {
    fn from(err: color_eyre::Report) -> Self {
        Self::Internal(err)
    }
}

impl From<iroha_data_model::ParseError> for WebError {
    fn from(err: iroha_data_model::ParseError) -> Self {
        Self::Internal(err.into())
    }
}

mod pagination;

mod accounts {
    use super::{
        assets::AssetDTO, fmt, get, web, AppData, Context, FromStr, Paginated,
        PaginationQueryParams, Scope, Serialize, WebError,
    };
    use iroha_data_model::prelude::{
        Account, AccountId, FindAccountById, FindAllAccounts, Metadata,
    };
    use serde::de;

    #[derive(Serialize)]
    pub struct AccountDTO {
        id: String,
        assets: Vec<AssetDTO>,
        metadata: Metadata,
        roles: Vec<String>,
    }

    impl From<Account> for AccountDTO {
        fn from(account: Account) -> Self {
            let assets: Vec<AssetDTO> = account
                .assets()
                .into_iter()
                .map(|asset|
                    // FIXME clone
                    AssetDTO::from(asset.clone()
                  ))
                .collect();

            let roles: Vec<String> = account
                .roles()
                .into_iter()
                .map(std::string::ToString::to_string)
                .collect();

            Self {
                id: account.id().to_string(),
                assets,
                metadata:
                // FIXME clone
                account.metadata().clone(),
                roles,
            }
        }
    }

    pub struct AccountIdInPath(pub AccountId);

    impl<'de> de::Deserialize<'de> for AccountIdInPath {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            struct Visitor;

            impl<'de> de::Visitor<'de> for Visitor {
                type Value = AccountIdInPath;

                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    write!(formatter, "a string in a format `alice@wonderland`")
                }

                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    AccountId::from_str(v)
                        .map(AccountIdInPath)
                        .map_err(|_parse_error| E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }

            deserializer.deserialize_string(Visitor)
        }
    }

    #[get("/{id}")]
    async fn show(
        data: web::Data<AppData>,
        id: web::Path<AccountIdInPath>,
    ) -> Result<web::Json<AccountDTO>, WebError> {
        let account = data
            .iroha_client
            .request(FindAccountById::new(id.into_inner().0))
            .await
            .map_err(WebError::expect_iroha_find_error)?;

        Ok(web::Json(account.into()))
    }

    #[get("")]
    async fn index(
        data: web::Data<AppData>,
        web::Query(pagination): web::Query<PaginationQueryParams>,
    ) -> Result<web::Json<Paginated<Vec<AccountDTO>>>, WebError> {
        let paginated: Paginated<_> = data
            .iroha_client
            .request_with_pagination(FindAllAccounts::new(), pagination.into())
            .await
            .wrap_err("Failed to request for accounts")?
            .try_into()?;

        Ok(web::Json(paginated.map(|accounts| {
            accounts.into_iter().map(Into::into).collect()
        })))
    }

    pub fn service() -> Scope {
        web::scope("/accounts").service(index).service(show)
    }
}

mod domains {
    use super::{
        accounts::AccountDTO, asset_definitions::AssetDefinitionDTO, get, web, AppData, Paginated,
        PaginationQueryParams, Scope, Serialize, WebError,
    };
    use iroha_data_model::prelude::{Domain, DomainId, FindAllDomains, FindDomainById, Metadata};

    #[derive(Serialize)]
    struct DomainDTO {
        id: String,
        accounts: Vec<AccountDTO>,
        logo: Option<String>,
        metadata: Metadata,
        asset_definitions: Vec<AssetDefinitionDTO>,
        // TODO amount of triggers
        triggers: u32,
    }

    impl From<Domain> for DomainDTO {
        fn from(domain: Domain) -> Self {
            Self {
                id: domain.id().to_string(),
                accounts: domain
                    .accounts()
                    .into_iter()
                    .map(|acc|
                        // FIXME clone
                        AccountDTO::from(acc.clone()))
                    .collect(),
                logo: domain.logo().as_ref().map(|x| x.as_ref().to_owned()),
                metadata: domain.metadata().clone(), // FIXME clone
                asset_definitions: AssetDefinitionDTO::vec_from_map(
                    domain
                        // FIXME clone
                        .asset_definitions()
                        .cloned(),
                ),
                triggers: 0,
            }
        }
    }

    #[get("/{id}")]
    async fn show(
        data: web::Data<AppData>,
        path: web::Path<String>,
    ) -> Result<web::Json<DomainDTO>, WebError> {
        let domain_id: DomainId = path.into_inner().parse()?;
        let domain = data
            .iroha_client
            .request(FindDomainById::new(domain_id))
            .await
            .map_err(WebError::expect_iroha_find_error)?;
        Ok(web::Json(DomainDTO::from(domain)))
    }

    #[get("")]
    async fn index(
        data: web::Data<AppData>,
        pagination: web::Query<PaginationQueryParams>,
    ) -> Result<web::Json<Paginated<Vec<DomainDTO>>>, WebError> {
        let paginated: Paginated<_> = data
            .iroha_client
            .request_with_pagination(FindAllDomains::new(), pagination.into_inner().into())
            .await
            .map_err(WebError::expect_iroha_any_error)?
            .try_into()?;
        Ok(web::Json(paginated.map(|domains| {
            domains.into_iter().map(Into::into).collect()
        })))
    }

    pub fn service() -> Scope {
        web::scope("/domains").service(index).service(show)
    }
}

mod assets {
    use super::{
        accounts::AccountIdInPath, asset_definitions::AssetDefinitionIdInPath, get, web, AppData,
        Paginated, PaginationQueryParams, Scope, Serialize, WebError,
    };
    use iroha_data_model::prelude::{
        Asset, AssetId, AssetValue, AssetValueType, FindAllAssets, FindAssetById, Metadata,
    };
    use serde::Deserialize;

    #[derive(Serialize)]
    #[serde(tag = "t", content = "c")]
    pub enum AssetValueDTO {
        Quantity(u32),
        BigQuantity(u128),
        Fixed(String),
        Store(Metadata),
    }

    impl From<AssetValue> for AssetValueDTO {
        fn from(val: AssetValue) -> Self {
            use AssetValue::{BigQuantity, Fixed, Quantity, Store};

            match val {
                Quantity(x) => Self::Quantity(x),
                BigQuantity(x) => Self::BigQuantity(x),
                Fixed(x) => Self::Fixed(f64::from(x).to_string()),
                Store(x) => Self::Store(x),
            }
        }
    }

    #[derive(Serialize)]
    pub struct AssetDTO {
        account_id: String,
        definition_id: String,
        value: AssetValueDTO,
    }

    impl From<Asset> for AssetDTO {
        fn from(val: Asset) -> Self {
            let id = val.id();
            // FIXME clone
            let value = val.value().clone();

            Self {
                account_id: id.account_id.to_string(),
                definition_id: id.definition_id.to_string(),
                value: AssetValueDTO::from(value),
            }
        }
    }

    #[derive(Serialize)]
    pub struct AssetValueTypeDTO(AssetValueType);

    #[derive(Deserialize)]
    pub struct AssetIdInPath {
        pub account_id: AccountIdInPath,
        pub definition_id: AssetDefinitionIdInPath,
    }

    impl From<AssetIdInPath> for AssetId {
        fn from(val: AssetIdInPath) -> Self {
            AssetId::new(val.definition_id.0, val.account_id.0)
        }
    }

    #[get("")]
    async fn index(
        data: web::Data<AppData>,
        pagination: web::Query<PaginationQueryParams>,
    ) -> Result<web::Json<Paginated<Vec<AssetDTO>>>, WebError> {
        let data: Paginated<_> = data
            .iroha_client
            .request_with_pagination(FindAllAssets::new(), pagination.into_inner().into())
            .await
            .map_err(WebError::expect_iroha_any_error)?
            .try_into()?;
        Ok(web::Json(data.map(|assets| {
            assets.into_iter().map(Into::into).collect()
        })))
    }

    #[get("/{definition_id}/{account_id}")]
    async fn show(
        data: web::Data<AppData>,
        path: web::Path<AssetIdInPath>,
    ) -> Result<web::Json<AssetDTO>, WebError> {
        let asset_id: AssetId = path.into_inner().into();
        let asset = data
            .iroha_client
            .request(FindAssetById::new(asset_id))
            .await
            .map_err(WebError::expect_iroha_find_error)?;
        Ok(web::Json(asset.into()))
    }

    pub fn service() -> Scope {
        web::scope("/assets").service(index).service(show)
    }
}

mod asset_definitions {
    use super::{
        fmt, get, web, AppData, FromStr, Paginated, PaginationQueryParams, Scope, Serialize,
        WebError,
    };
    use iroha_data_model::{
        asset::Mintable,
        prelude::{
            AssetDefinition, AssetDefinitionEntry, AssetDefinitionId, AssetValueType,
            FindAllAssetsDefinitions,
        },
    };
    use serde::de;

    #[derive(Serialize)]
    pub struct AssetDefinitionDTO {
        id: String,
        value_type: AssetValueTypeDTO,
        mintable: Mintable,
    }

    impl AssetDefinitionDTO {
        pub fn vec_from_map<T>(map: T) -> Vec<Self>
        where
            T: ExactSizeIterator + Iterator<Item = AssetDefinitionEntry>,
        {
            map.into_iter()
                .map(|def| def.definition().clone().into())
                .collect()
        }
    }

    impl From<AssetDefinition> for AssetDefinitionDTO {
        fn from(definition: AssetDefinition) -> Self {
            Self {
                id: definition.id().to_string(),
                value_type: AssetValueTypeDTO(*definition.value_type()),
                mintable: *definition.mintable(),
            }
        }
    }

    pub struct AssetDefinitionIdInPath(pub AssetDefinitionId);

    impl<'de> de::Deserialize<'de> for AssetDefinitionIdInPath {
        fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
        where
            D: de::Deserializer<'de>,
        {
            struct Visitor;

            impl<'de> de::Visitor<'de> for Visitor {
                type Value = AssetDefinitionIdInPath;

                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    write!(formatter, "a string in a format `rose#wonderland`")
                }

                fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
                where
                    E: de::Error,
                {
                    AssetDefinitionId::from_str(v)
                        .map(AssetDefinitionIdInPath)
                        .map_err(|_parse_error| E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }

            deserializer.deserialize_string(Visitor)
        }
    }

    #[derive(Serialize)]
    pub struct AssetValueTypeDTO(AssetValueType);

    // WIP iroha does not support FindAssetDefinitionById yet
    // https://github.com/hyperledger/iroha/pull/2126
    // #[get("/{id}")]
    // async fn show(id: web::Path<AssetDefinitionIdInPath>) -> Result<impl Responder, WebError> {}

    #[get("")]
    async fn index(
        data: web::Data<AppData>,
        pagination: web::Query<PaginationQueryParams>,
    ) -> Result<web::Json<Paginated<Vec<AssetDefinitionDTO>>>, WebError> {
        let data: Paginated<_> = data
            .iroha_client
            .request_with_pagination(FindAllAssetsDefinitions::new(), pagination.0.into())
            .await
            .map_err(WebError::expect_iroha_any_error)?
            .try_into()?;
        Ok(web::Json(
            data.map(|items| items.into_iter().map(Into::into).collect()),
        ))
    }

    pub fn service() -> Scope {
        web::scope("/asset-definitions")
            // .service(show)
            .service(index)
    }
}

mod peer {
    use super::{get, web, AppData, Paginated, PaginationQueryParams, Scope, Serialize, WebError};
    use iroha_data_model::prelude::{FindAllPeers, Peer, PeerId};
    use iroha_telemetry::metrics::Status;

    #[derive(Serialize)]
    pub struct PeerDTO(PeerId);

    impl From<Peer> for PeerDTO {
        fn from(val: Peer) -> Self {
            Self(val.id)
        }
    }

    #[get("/peers")]
    async fn peers(
        data: web::Data<AppData>,
        pagination: web::Query<PaginationQueryParams>,
    ) -> Result<web::Json<Paginated<Vec<PeerDTO>>>, WebError> {
        let data: Paginated<_> = data
            .iroha_client
            .request_with_pagination(FindAllPeers::new(), pagination.0.into())
            .await
            .map_err(WebError::expect_iroha_any_error)?
            .try_into()?;
        Ok(web::Json(
            data.map(|items| items.into_iter().map(Into::into).collect()),
        ))
    }

    #[get("/status")]
    async fn status(data: web::Data<AppData>) -> Result<web::Json<Status>, WebError> {
        let status = data.iroha_client.get_status().await?;
        Ok(web::Json(status))
    }

    pub fn service() -> Scope {
        web::scope("/peer").service(peers).service(status)
    }
}

mod roles {
    use super::{get, web, AppData, Paginated, PaginationQueryParams, Scope, Serialize, WebError};
    use iroha_data_model::prelude::{FindAllRoles, Role};

    #[derive(Serialize)]
    pub struct RoleDTO(Role);

    impl From<Role> for RoleDTO {
        fn from(val: Role) -> Self {
            Self(val)
        }
    }

    #[get("")]
    async fn index(
        app: web::Data<AppData>,
        pagination: web::Query<PaginationQueryParams>,
    ) -> Result<web::Json<Paginated<Vec<RoleDTO>>>, WebError> {
        let data: Paginated<_> = app
            .iroha_client
            // TODO add an issue about absense of `FindAllRoles::new()`?
            .request_with_pagination(FindAllRoles {}, pagination.0.into())
            .await
            .map_err(WebError::expect_iroha_any_error)?
            .try_into()?;
        Ok(web::Json(
            data.map(|items| items.into_iter().map(Into::into).collect()),
        ))
    }

    pub fn service() -> Scope {
        web::scope("/roles").service(index)
    }
}

async fn default_route() -> impl Responder {
    HttpResponse::NotFound().body("Not Found")
}

#[get("")]
async fn root_health_check() -> impl Responder {
    HttpResponse::Ok().body("Welcome to Iroha 2 Block Explorer!")
}

pub struct ServerInitData {
    iroha_client: Arc<iroha_client::client::Client>,
}

impl ServerInitData {
    pub fn new(iroha_client: Arc<iroha_client::client::Client>) -> Self {
        Self { iroha_client }
    }
}

/// Initializes a server listening on `127.0.0.1:<port>`. It should be awaited to be actually started.
pub fn server(
    ServerInitData { iroha_client }: ServerInitData,
    port: u16,
) -> color_eyre::Result<actix_server::Server> {
    let server = HttpServer::new(move || {
        let client_wrap = crate::iroha_client_wrap::IrohaClientWrap::new(iroha_client.clone());
        let app_data = web::Data::new(AppData::new(client_wrap));

        App::new()
            .app_data(app_data)
            .app_data(web::QueryConfig::default().error_handler(|err, _req| {
                WebError::BadRequest(format!("Bad query: {err}")).into()
            }))
            .wrap(super::logger::TracingLogger::default())
            .wrap(middleware::NormalizePath::new(
                middleware::TrailingSlash::Trim,
            ))
            .service(
                web::scope("/api/v1")
                    .service(root_health_check)
                    .service(accounts::service())
                    .service(domains::service())
                    .service(assets::service())
                    .service(asset_definitions::service())
                    .service(roles::service())
                    .service(peer::service()),
            )
            .default_service(web::route().to(default_route))
    })
    .bind(("127.0.0.1", port))?
    .run();

    Ok(server)
}
