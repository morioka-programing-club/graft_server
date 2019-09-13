use actix_web::{web, error, http::StatusCode, Error as ActixError};
use std::io::{self, ErrorKind};
use futures_locks::Mutex;
use serde_json::Value;
use std::error::Error;
use actix::prelude::*;
use tokio_postgres::{connect, NoTls, Statement, Client, Row, types::{Type, Kind, IsNull, ToSql}};
use futures::future;

#[derive(Debug)]
pub enum ActorVariant {
	User,
	Group
}

// Manually expanded https://github.com/sfackler/rust-postgres-derive since it didn't work with tokio-postgres
impl ToSql for ActorVariant {
	fn to_sql(&self, _type: &Type, buf: &mut Vec<u8>) -> Result<IsNull, Box<Error + Sync + Send>> {
		let s = match self {
        	ActorVariant::User => "member",
			ActorVariant::Group => "organization"
    	};

    	buf.extend_from_slice(s.as_bytes());
    	Ok(IsNull::No)
	}

    fn accepts(type_: &Type) -> bool {
		if type_.name() != "actors_available" {
            return false;
        }

        match *type_.kind() {
            Kind::Enum(ref variants) => {
                if variants.len() != 2 {
                    return false;
                }

                variants.iter().all(|v| {
                    match &**v {
                        "member" => true,
                        "organization" => true,
                        _ => false
                    }
                })
            }
            _ => false
        }
    }

    fn to_sql_checked(&self, type_: &Type, buf: &mut Vec<u8>) -> Result<IsNull, Box<Error + Sync + Send>> {
		self.to_sql(type_, buf)
	}
}

pub fn into_value(row: &Row, name: &str, col_type: &Type) -> Value {
	macro_rules! from_sql {
		($(($sql_type:ident, $type_to:ty)),*) => {
			match col_type {
				$(&Type::$sql_type => row.get::<&str, Option<$type_to>>(name).map_or(Value::Null, |v| v.into()),)*
				_ => panic!("Specified SQL cell's type is not compatible to JSON")
			}
		}
	}
	from_sql![
		(CHAR, i8),
		(INT2, i16),
		(INT4, i32),
		(INT8, i64),
		(OID, u32),
		(FLOAT4, f32),
		(FLOAT8, f64),
		(BYTEA, &[u8]),
		(TEXT, &str),
		(BOOL, bool)
	]
}

pub struct Db {
	client: Client,
	statements: Statements
}

impl Actor for Db {
    type Context = Context<Self>;
}

impl Db {
	pub fn get(&mut self) -> (&mut Client, &Statements) {
		(&mut self.client, &self.statements)
	}
}

pub struct Statements {
	pub get_inbox: Statement,
	pub get_outbox: Statement,
	pub create_message: Statement,
	pub add_sender: Statement,
	pub add_reciever: Statement,
	pub create_actor: Statement,
	pub delete_actor: Statement
}

pub type DbWrapper = web::Data<Mutex<Db>>;

pub fn process_senders(json: Value, id: i64, db: DbWrapper) -> Box<Future<Item = (), Error = ActixError>> {
	match json {
		Value::String(str) => {
			Box::new(db.lock().from_err().join(future::ok(str)).and_then(move |(mut db_locked, str)| {
				let (client, statements) = db_locked.get();
				client.execute(&statements.add_sender, &[&id, &str])
					.map(|_| ()).map_err(error::ErrorInternalServerError)
			}))
		},
		Value::Object(obj) => {
			Box::new(db.lock().from_err().join(future::ok(obj)).and_then(move |(mut db_locked, obj)| {
				let (client, statements) = db_locked.get();
				client.execute(&statements.add_sender, &[&id, &obj["id"].as_str()])
					.map(|_| ()).map_err(error::ErrorInternalServerError)
			}))
		},
		Value::Array(arr) => Box::new(
			future::join_all(arr.to_owned().into_iter().map(move |el| process_senders(el, id, db.clone()))).map(|_| ())
				.map_err(|e| error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR).into())
		),
		_ => Box::new(future::err(error::ErrorBadRequest("Invaild actor")))
	}
}

pub fn process_recievers(json: Value, id: i64, db: DbWrapper) -> Box<Future<Item = (), Error = ActixError>> {
	match json {
		Value::String(str) => {
			Box::new(db.lock().from_err().join(future::ok(str)).and_then(move |(mut db_locked, str)| {
				let (client, statements) = db_locked.get();
				client.execute(&statements.add_reciever, &[&id, &str])
					.map(|_| ()).map_err(error::ErrorInternalServerError)
			}))
		},
		Value::Object(obj) => {
			Box::new(db.lock().from_err().join(future::ok(obj)).and_then(move |(mut db_locked, obj)| {
				let (client, statements) = db_locked.get();
				client.execute(&statements.add_reciever, &[&id, &obj["id"].as_str()])
					.map(|_| ()).map_err(error::ErrorInternalServerError)
			}))
		},
		Value::Array(arr) => Box::new(
			future::join_all(arr.to_owned().into_iter().map(move |el| process_recievers(el, id, db.clone()))).map(|_| ())
				.map_err(|e| error::InternalError::new(e, StatusCode::INTERNAL_SERVER_ERROR).into())
		),
		_ => Box::new(future::err(error::ErrorBadRequest("Invaild actor")))
	}
}

pub fn init(user_name: &str) -> Box<Future<Item = DbWrapper, Error = io::Error>> {
	Box::new(
		connect(&(String::from("postgres://") + user_name + "@localhost/graft"), NoTls)
			.map_err(|e| io::Error::new(ErrorKind::Other, e))
			.and_then(move |(mut cl, conn)| {
				Arbiter::spawn(conn.map_err(|e| panic!("{}", e)));
				future::join_all(vec![
					// Insert SQL statements here
					cl.prepare("SELECT * FROM messages WHERE id IN (SELECT message FROM messages_recieved WHERE actor = $1) ORDER BY ctime;"), // get inbox
					cl.prepare("SELECT * FROM messages WHERE id IN (SELECT message FROM messages_sent WHERE actor = $1) ORDER BY ctime;"), // get outbox
					cl.prepare("INSERT INTO messages (content, ctime, mtime) VALUES ($1, $2, $2) RETURNING id;"), // create message
					cl.prepare("INSERT INTO messages_sent (actor, message) VALUES ($2, $1);"), // add sender
					cl.prepare("INSERT INTO messages_recieved (actor, message) VALUES ($2, $1);"), // add reciever
					cl.prepare("INSERT INTO actors (actortype, id) VALUES ($1, $2);"), // create actor
					cl.prepare("DELETE FROM actors WHERE actortype = $2 AND id = $1;") // delete actor
				]).and_then(move |statements| {
					println!("SQL Statements prepared successfully");
					let mut iter = statements.into_iter();
					Ok(web::Data::new(Mutex::new(Db {
						client: cl,
						statements: Statements {
							get_inbox: iter.next().unwrap(),
							get_outbox: iter.next().unwrap(),
							create_message: iter.next().unwrap(),
							add_sender: iter.next().unwrap(),
							add_reciever: iter.next().unwrap(),
							create_actor: iter.next().unwrap(),
							delete_actor: iter.next().unwrap()
						}
					})))
				}).map_err(|e| io::Error::new(ErrorKind::Other, e))
			})
	)
}