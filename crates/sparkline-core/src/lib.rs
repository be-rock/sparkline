//! Core of sparkline: a Spark Connect client plus the SQL tooling (lexer,
//! statement splitting, completion) shared by the terminal front-ends.

pub mod catalog;
pub mod client;
pub mod complete;
pub mod conn;
pub mod error;
pub mod keywords;
pub mod lexer;
pub mod proto;
pub mod suggest;

pub use client::{ExecEvent, Execution, Progress, SparkClient};
pub use conn::ConnectionString;
pub use error::{ClientError, SparkError};
