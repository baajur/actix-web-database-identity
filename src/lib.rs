//! A database (SQL) Identity Provider
//!
//! Provides a way to interact with SQL databases with actix-web's
//! identity policy.
//!
//! # Example
//!
//! ```no_run
//! extern crate actix_web;
//! extern crate actix_web_sql_identity;
//!
//! use actix_web::{http, server, App, HttpRequest, Responder};
//! use actix_web::middleware::identity::{IdentityService, RequestIdentity};
//! use actix_web_sql_identity::SqlIdentityBuilder;
//!
//! const POOL_SIZE: usize = 3;     // Number of connections per pool
//!
//! fn login(mut req: HttpRequest) -> impl Responder {
//!     // Should pull username/id from request
//!     req.remember("username_or_id".to_string());
//!     "Logged in!".to_string()
//! }
//!
//! fn profile(req: HttpRequest) -> impl Responder {
//!     if let Some(user) = req.identity() {
//!         format!("Hello, {}!", user)
//!     } else {
//!         "Hello, anonymous user!".to_string()
//!     }
//! }
//!
//! fn logout(mut req: HttpRequest) -> impl Responder {
//!     req.forget();
//!    "Logged out!".to_string()
//! }
//!
//! fn main() {
//!     server::new(|| {
//!         let policy = SqlIdentityBuilder::new("sqlite://my.db")
//!             .pool_size(POOL_SIZE);
//!
//!         App::new()
//!            .route("/login", http::Method::POST, login)
//!            .route("/profile", http::Method::GET, profile)
//!            .route("/logout", http::Method::POST, logout)
//!            .middleware(IdentityService::new(
//!                     policy.finish()
//!                         .expect("failed to connect to database")))
//!     })
//!     .bind("127.0.0.1:7070").unwrap()
//!     .run();
//! }
//! ```
extern crate actix;
extern crate actix_web;
extern crate base64;
extern crate chrono;
extern crate failure;
extern crate futures;
extern crate rand;

#[macro_use]
extern crate diesel;
#[macro_use]
extern crate failure_derive;
#[macro_use]
extern crate log;

mod sql;

use chrono::prelude::Utc;
use chrono::NaiveDateTime;

use std::rc::Rc;

use failure::Error;

use actix::Addr;

// Actix Web imports
use actix_web::error::{self, Error as ActixWebError};
use actix_web::http::header::HeaderValue;
use actix_web::middleware::identity::{Identity, IdentityPolicy};
use actix_web::middleware::Response as MiddlewareResponse;
use actix_web::{HttpMessage, HttpRequest, HttpResponse};

// Futures imports
use futures::future::{err as FutErr, ok as FutOk};
use futures::Future;

// (Local) Sql Imports
use sql::{DeleteIdentity, FindIdentity, SqlActor, SqlIdentityModel, UpdateIdentity, Variant};

// Rand Imports (thread secure!)
use rand::Rng;

const DEFAULT_RESPONSE_HDR: &'static str = "X-Actix-Auth";
const DEFAULT_POOL_SIZE: usize = 3;

/// Error representing different failure cases
#[derive(Debug, Fail)]
enum SqlIdentityError {
    #[allow(dead_code)]
    #[fail(display = "sql variant not supported")]
    SqlVariantNotSupported,

    #[fail(display = "token not found")]
    TokenNotFound,

    #[fail(display = "token failed to set in header")]
    TokenNotSet,

    #[fail(display = "token not provided but required, bad request")]
    TokenRequired,
}

enum SqlIdentityState {
    Created,
    Updated,
    Deleted,
    Unchanged,
}

/// Identity that uses a SQL database as identity storage
pub struct SqlIdentity {
    id: i64,
    state: SqlIdentityState,
    identity: Option<String>,
    token: Option<String>,
    ip: Option<String>,
    user_agent: Option<String>,
    created: NaiveDateTime,
    inner: Rc<SqlIdentityInner>,
}

impl Identity for SqlIdentity {
    /// Returns the current identity, or none
    fn identity(&self) -> Option<&str> {
        self.identity.as_ref().map(|s| s.as_ref())
    }

    /// Remembers a given user (by setting a token value)
    ///
    /// # Arguments
    ///
    /// * `value` - User to remember
    fn remember(&mut self, value: String) {
        self.identity = Some(value);

        // Generate a random token
        let mut arr = [0u8; 24];
        rand::thread_rng().fill(&mut arr[..]);
        self.token = Some(base64::encode(&arr));

        self.state = SqlIdentityState::Created;
    }

    /// Forgets a user, by deleting the identity
    fn forget(&mut self) {
        self.identity = None;
        self.state = SqlIdentityState::Deleted;
    }

    /// Saves the identity to the backing store, if it has changed
    ///
    /// # Arguments
    ///
    /// * `resp` - HTTP response to modify
    fn write(&mut self, resp: HttpResponse) -> Result<MiddlewareResponse, ActixWebError> {
        match self.state {
            SqlIdentityState::Created => {
                self.state = SqlIdentityState::Unchanged;
                Ok(MiddlewareResponse::Future(self.inner.create(self, resp)))
            },

            SqlIdentityState::Updated if self.token.is_some() && self.identity.is_some() => {
                self.state = SqlIdentityState::Unchanged;
                Ok(MiddlewareResponse::Future(self.inner.save(self, resp)))
            }

            SqlIdentityState::Deleted if self.token.is_some() => {
                let token = self.token.as_ref().expect("[SIS::Deleted] Token is None!");
                self.state = SqlIdentityState::Unchanged;
                Ok(MiddlewareResponse::Future(self.inner.remove(token, resp)))
            }

            SqlIdentityState::Deleted | SqlIdentityState::Updated => {
                // Not logged in/log in failed
                Err(error::ErrorBadRequest(SqlIdentityError::TokenRequired))
            }

            _ => {
                self.state = SqlIdentityState::Unchanged;
                Ok(MiddlewareResponse::Done(resp))
            }
        }
    }
}

/// Wrapped inner-provider for SQL storage
struct SqlIdentityInner {
    addr: Addr<SqlActor>,
    hdr: &'static str,
}

impl SqlIdentityInner {
    /// Creates a new instance of a SqlIdentityInner struct
    ///
    /// # Arguments
    ///
    /// * `addr` - A SQL connection, already opened
    fn new(addr: Addr<SqlActor>, hdr: &'static str) -> SqlIdentityInner {
        SqlIdentityInner { addr, hdr }
    }

    fn create(&self, identity: &SqlIdentity, mut resp: HttpResponse) -> Box<Future<Item = HttpResponse, Error = ActixWebError>> {
        if let Some(ref token) = identity.token {
            let headers = resp.headers_mut();

            if let Ok(token) = token.parse() {
                headers.append(self.hdr, token);
            } else {
                error!("Failed to parse token to place in header!");
                return Box::new(FutErr(error::ErrorInternalServerError(
                    SqlIdentityError::TokenNotSet,
                )));
            }
        } else {
            error!("Identity token not set!");
            return Box::new(FutErr(error::ErrorUnauthorized(
                SqlIdentityError::TokenNotFound,
            )));
        }

        Box::new(
            self.addr
                .send(UpdateIdentity::create(identity))
                .map_err(ActixWebError::from)
                .and_then(move |res| match res {
                    Ok(_) => Ok(resp),
                    Err(e) => {
                        error!("ERROR: {:?}", e);
                        Err(error::ErrorInternalServerError(e))
                    }
                }),
        )
    }

    /// Saves an identity to the backend provider (SQL database)
    fn save(
        &self,
        identity: &SqlIdentity,
        resp: HttpResponse,
    ) -> Box<Future<Item = HttpResponse, Error = ActixWebError>> {

        Box::new(
            self.addr
                .send(UpdateIdentity::update(identity))
                .map_err(ActixWebError::from)
                .and_then(move |res| match res {
                    Ok(_) => Ok(resp),
                    Err(e) => {
                        error!("ERROR: {:?}", e);
                        Err(error::ErrorInternalServerError(e))
                    }
                }),
        )
    }

    /// Removes an identity from the backend provider (SQL database)
    fn remove(
        &self,
        token: &str,
        resp: HttpResponse,
    ) -> Box<Future<Item = HttpResponse, Error = ActixWebError>> {
        Box::new(
            self.addr
                .send(DeleteIdentity {
                    token: token.to_string(),
                })
                .map_err(ActixWebError::from)
                .and_then(move |res| match res {
                    Ok(_) => Ok(resp),
                    Err(e) => {
                        error!("ERROR: {:?}", e);
                        Err(error::ErrorInternalServerError(e))
                    }
                }),
        )
    }

    /// Loads an identity from the backend provider (SQL database)
    fn load<S>(
        &self,
        req: &HttpRequest<S>,
    ) -> Box<Future<Item = Option<SqlIdentityModel>, Error = ActixWebError>> {
        let headers = req.headers();
        let auth_header = headers.get("Authorization");

        if let Some(auth_header) = auth_header {
            // Return the identity (or none, if it doesn't exist)

            if let Ok(auth_header) = auth_header.to_str() {
                let mut iter = auth_header.split(' ');
                let scheme = iter.next();
                let token = iter.next();

                if scheme.is_some() && token.is_some() {
                    let _scheme = scheme.expect("[SII::load] Scheme is None!");
                    let token = token.expect("[SII::load] Token is None!");

                    return Box::new(
                        self.addr
                            .send(FindIdentity {
                                token: token.to_string(),
                            })
                            .map_err(ActixWebError::from)
                            .and_then(move |res| match res {
                                Ok(val) => Ok(Some(val)),
                                Err(e) => {
                                    warn!("WARN: {:?}", e);
                                    Ok(None)
                                }
                            }),
                    );
                }
            }
        }

        Box::new(FutOk(None))
    }
}

/// Use a SQL database for request identity storage
#[derive(Clone)]
pub struct SqlIdentityPolicy(Rc<SqlIdentityInner>);

#[derive(Clone)]
pub struct SqlIdentityBuilder {
    pool: usize,
    uri: String,
    hdr: &'static str,
    variant: Variant,
}

impl SqlIdentityBuilder {
    /// Creates a new SqlIdentityBuilder that constructs a SqlIdentityPolicy
    ///
    /// # Arguments
    ///
    /// * `uri` - Database connection string
    ///
    /// # Example
    ///
    /// ```no_run
    /// # extern crate actix_web;
    /// # extern crate actix_web_sql_identity;
    ///
    /// use actix_web::App;
    /// use actix_web::middleware::identity::IdentityService;
    /// use actix_web_sql_identity::SqlIdentityBuilder;
    ///
    /// // Create the identity policy
    /// let policy = SqlIdentityBuilder::new("postgres://user:pass@host/database")
    ///                 .pool_size(5)
    ///                 .response_header("X-MY-RESPONSE")
    ///                 .finish()
    ///                 .expect("failed to open database");
    ///
    /// let app = App::new().middleware(IdentityService::new(
    ///     policy
    /// ));
    /// ```
    pub fn new<T: Into<String>>(uri: T) -> SqlIdentityBuilder {
        let uri: String = uri.into();
        let variant = SqlIdentityBuilder::variant(&uri);

        SqlIdentityBuilder {
            pool: DEFAULT_POOL_SIZE,
            uri: uri,
            hdr: DEFAULT_RESPONSE_HDR,
            variant: variant,
        }
    }

    fn variant(uri: &str) -> Variant {
        if uri.starts_with("mysql://") {
            return Variant::Mysql;
        } else if uri.starts_with("postgres://") || uri.starts_with("postgresql://") {
            return Variant::Pg;
        } else {
            return Variant::Sqlite;
        }
    }

    /// Change the response header when an identity is remembered
    ///
    /// # Arguments
    ///
    /// * `hdr` - Response header name to use
    pub fn response_header(mut self, hdr: &'static str) -> SqlIdentityBuilder {
        self.hdr = hdr;
        self
    }

    /// Change how many SQL connections are in each pool
    ///
    /// # Arguments
    ///
    /// * `count` - Number of connections per pool
    pub fn pool_size(mut self, count: usize) -> SqlIdentityBuilder {
        self.pool = count;
        self
    }

    /// Finish building this SQL identity policy.  This will attempt
    /// to construct the a pool of connections to the database
    /// specified.  The type of database is determined by the uri set.
    /// On success, a new SqlIdentityPolicy is returned, on failure
    /// an error is returned.
    pub fn finish(self) -> Result<SqlIdentityPolicy, Error> {
        info!("Registering identity provider: {:?}", self.variant);

        Ok(SqlIdentityPolicy(Rc::new(SqlIdentityInner::new(
            match self.variant {
                Variant::Sqlite => SqlActor::sqlite(self.pool, &self.uri)?,
                Variant::Mysql => SqlActor::mysql(self.pool, &self.uri)?,
                Variant::Pg => SqlActor::pg(self.pool, &self.uri)?,
            },
            self.hdr,
        ))))
    }

    /// Forces a SQLite identity policy to be created.  This function does
    /// not normally need to be used, `new` will automatically determine
    /// the appropriate variant by parsing the connection string.  This function
    /// exists if the parsing fails
    pub fn sqlite(mut self) -> SqlIdentityBuilder {
        self.variant = Variant::Sqlite;
        self
    }

    /// Forces a MySQL identity policy to be created.  This function does
    /// not normally need to be used, `new` will automatically determine
    /// the appropriate variant by parsing the connection string.  This function
    /// exists if the parsing fails
    pub fn mysql(mut self) -> SqlIdentityBuilder {
        self.variant = Variant::Mysql;
        self
    }

    /// Forces a PostgreSQL identity policy to be created.  This function does
    /// not normally need to be used, `new` will automatically determine
    /// the appropriate variant by parsing the connection string.  This function
    /// exists if the parsing fails
    pub fn postgresql(mut self) -> SqlIdentityBuilder {
        self.variant = Variant::Pg;
        self
    }
}

impl<S> IdentityPolicy<S> for SqlIdentityPolicy {
    type Identity = SqlIdentity;
    type Future = Box<Future<Item = SqlIdentity, Error = ActixWebError>>;

    /// Process a request recieved by the server, returns an Identity struct
    ///
    /// # Arguments
    ///
    /// * `req` - The HTTP request recieved
    fn from_request(&self, req: &HttpRequest<S>) -> Self::Future {
        let inner = Rc::clone(&self.0);
        let conn_ip = req.connection_info()
            .remote()
            .map_or("0.0.0.0", |s| s.split(":").nth(0).unwrap())
            .to_owned();

        let unk = HeaderValue::from_static("Unknown");
        let ua = req.headers()
            .get("user-agent")
            .unwrap_or(&unk)
            .to_str()
            .unwrap_or("Unknown")
            .to_owned();

        Box::new(self.0.load(req).map(move |ident| {
            if let Some(id) = ident {

                SqlIdentity {
                    id: id.id,
                    identity: Some(id.userid),
                    token: Some(id.token),
                    ip: Some(conn_ip),
                    user_agent: Some(ua),
                    created: id.created,
                    state: SqlIdentityState::Updated,
                    inner: inner,
                }
            } else {
                SqlIdentity {
                    id: -1,
                    identity: None,
                    token: None,
                    ip: Some(conn_ip),
                    user_agent: Some(ua),
                    created: Utc::now().naive_utc(),
                    state: SqlIdentityState::Unchanged,
                    inner: inner,
                }
            }
        }))
    }
}
