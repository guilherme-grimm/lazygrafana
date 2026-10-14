//! Grafana API access: HTTP client, response models, data frames,
//! dashboard model and template variables.

pub mod client;
pub mod dashboard;
pub mod dataframe;
pub mod models;
pub mod variables;

pub use client::{ApiError, ApiResult, Client, QueryRequest, QueryResponse};
