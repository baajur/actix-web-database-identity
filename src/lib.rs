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
//!         let policy = SqlIdentityBuilder::new("my.db")
//!             .pool_size(POOL_SIZE);
//!
//!         App::new()
//!            .route("/login", http::Method::POST, login)
//!            .route("/profile", http::Method::GET, profile)
//!            .route("/logout", http::Method::POST, logout)
//!            .middleware(IdentityService::new(
//!                     policy.sqlite()
//!                         .expect("failed to connect to database")))
//!     })
//!     .bind("127.0.0.1:7070").unwrap()
//!     .run();
//! }
//! ```

extern crate actix;
extern crate actix_web;
extern crate base64;
extern crate failure;
extern crate futures;
extern crate rand;

#[macro_use]
extern crate diesel;
#[macro_use]
extern crate failure_derive;

mod sql;

use std::rc::Rc;

use failure::Error;

use actix::prelude::Syn;
use actix::Addr;

// Actix Web imports
use actix_web::error::Error as ActixWebError;
use actix_web::middleware::identity::{Identity, IdentityPolicy};
use actix_web::middleware::Response as MiddlewareResponse;
use actix_web::{HttpMessage, HttpRequest, HttpResponse};

// Futures imports
use futures::future::ok as FutOk;
use futures::Future;

// (Local) Sql Imports
use sql::{DeleteIdentity, FindIdentity, SqlActor, SqlIdentityModel, UpdateIdentity};

// Rand Imports (thread secure!)
use rand::Rng;

const DEFAULT_RESPONSE_HDR: &'static str= "X-Actix-Auth";
const DEFAULT_POOL_SIZE: usize = 3;

/// Error representing different failure cases
#[derive(Debug, Fail)]
enum SqlIdentityError {
    #[allow(dead_code)]
    #[fail(display = "sql variant not supported")]
    SqlVariantNotSupported,

    #[fail(display = "token not found")]
    SqlTokenNotFound,
}

enum SqlIdentityState {
    Saved,
    Deleted,
    Unchanged,
}

/// Identity that uses a SQL database as identity storage
pub struct SqlIdentity {
    state: SqlIdentityState,
    identity: Option<String>,
    token: Option<String>,
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

        self.state = SqlIdentityState::Saved;
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
            SqlIdentityState::Saved if self.token.is_some() && self.identity.is_some() => {
                let token = self.token.as_ref().expect("[SIS::Saved] Token is None!");
                let identity = self.identity.as_ref().expect("[SIS::Saved] Identity is None!");
                self.state = SqlIdentityState::Unchanged;
                Ok(MiddlewareResponse::Future(
                    self.inner.save(token, identity, resp),
                ))
            }

            SqlIdentityState::Deleted if self.token.is_some() => {
                let token = self.token.as_ref().expect("[SIS::Deleted] Token is None!");
                self.state = SqlIdentityState::Unchanged;
                Ok(MiddlewareResponse::Future(self.inner.remove(token, resp)))
            }

            SqlIdentityState::Deleted | SqlIdentityState::Saved => {
                // Not logged in/log in failed
                Ok(MiddlewareResponse::Done(
                    HttpResponse::BadRequest().finish(),
                ))
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
    addr: Addr<Syn, SqlActor>,
    hdr: &'static str,
}

impl SqlIdentityInner {
    /// Creates a new instance of a SqlIdentityInner struct
    ///
    /// # Arguments
    ///
    /// * `addr` - A SQL connection, already opened
    fn new(addr: Addr<Syn, SqlActor>, hdr: &'static str) -> SqlIdentityInner {
        SqlIdentityInner { addr, hdr }
    }

    /// Saves an identity to the backend provider (SQL database)
    fn save(
        &self,
        token: &str,
        userid: &str,
        mut resp: HttpResponse,
    ) -> Box<Future<Item = HttpResponse, Error = ActixWebError>> {
        {
            // Add the new token/identity to response headers
            let headers = resp.headers_mut();
            headers.append(self.hdr, token.parse().expect("[SII::save] token failed to parse"));
        }

        Box::new(
            self.addr
                .send(UpdateIdentity {
                    token: token.to_string(),
                    userid: userid.to_string(),
                })
                .map_err(ActixWebError::from)
                .and_then(move |res| match res {
                    Ok(_) => Ok(resp),
                    Err(e) => {
                        println!("[SII::save] ERROR: {:?}", e);
                        Ok(HttpResponse::InternalServerError().finish())
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
                    Err(_) => Ok(HttpResponse::InternalServerError().finish()),
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
                                Err(_) => Ok(None),
                            }),
                    );
                }
            }
        }

        Box::new(FutOk(None))
    }
}

/// Use a SQL database for request identity storage
pub struct SqlIdentityPolicy(Rc<SqlIdentityInner>);

pub struct SqlIdentityBuilder {
    pool: usize,
    uri: String,
    hdr: &'static str,
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
    /// let policy = SqlIdentityBuilder::new("database.sqlite3")
    ///                 .pool_size(5)
    ///                 .sqlite()
    ///                 .expect("failed to open database");
    ///
    /// let app = App::new().middleware(IdentityService::new(
    ///     policy
    /// ));
    /// ```
    pub fn new<T: Into<String>>(uri: T) -> SqlIdentityBuilder {
        SqlIdentityBuilder {
            pool: DEFAULT_POOL_SIZE,
            uri: uri.into(),
            hdr: DEFAULT_RESPONSE_HDR,
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

    /// Creates a SQLite identity policy, returning the policy if successful,
    /// or an error if unsuccessful
    pub fn sqlite(self) -> Result<SqlIdentityPolicy, Error> {
        Ok(SqlIdentityPolicy(Rc::new(SqlIdentityInner::new(
                        SqlActor::sqlite(self.pool, &self.uri)?,
                        self.hdr,
        ))))
    }

    /// Creates a MySQL identity policy, returning the policy if successful,
    /// or an error if unsuccessful
    pub fn mysql(self) -> Result<SqlIdentityPolicy, Error> {
        Ok(SqlIdentityPolicy(Rc::new(SqlIdentityInner::new(
                        SqlActor::mysql(self.pool, &self.uri)?,
                        self.hdr,
        ))))
    }

    /// Creates a PostgreSQL identity policy, returning the policy if successful,
    /// or an error if unsuccessful
    pub fn postgresql(self) -> Result<SqlIdentityPolicy, Error> {
        Ok(SqlIdentityPolicy(Rc::new(SqlIdentityInner::new(
                        SqlActor::pg(self.pool, &self.uri)?,
                        self.hdr,
        ))))
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
    fn from_request(&self, req: &mut HttpRequest<S>) -> Self::Future {
        let inner = Rc::clone(&self.0);

        Box::new(self.0.load(req).map(move |ident| {
            if let Some(id) = ident {
                SqlIdentity {
                    identity: Some(id.userid),
                    token: Some(id.token),
                    state: SqlIdentityState::Unchanged,
                    inner: inner,
                }
            } else {
                SqlIdentity {
                    identity: None,
                    token: None,
                    state: SqlIdentityState::Unchanged,
                    inner: inner,
                }
            }
        }))
    }
}
