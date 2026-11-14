//! Generated Spark Connect protocol types (from the vendored `.proto` files).

#[allow(clippy::all, clippy::pedantic, missing_docs, deprecated)]
pub mod spark {
    pub mod connect {
        tonic::include_proto!("spark.connect");
    }
}

pub use spark::connect::*;
