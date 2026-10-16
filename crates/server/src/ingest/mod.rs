//! The ingest pipeline, named after the Ruby it ports:
//! Payload (wire decoding) -> ReceiptHeaders (idempotent deliveries) ->
//! Mapper (record -> row) -> Batch (validation, rejections) -> Writer (rows,
//! ledger and receipt in one SQLite transaction per tenant).

pub mod batch;
pub mod bounded_gzip;
pub mod mapper;
pub mod payload;
pub mod receipt_headers;
pub mod writer;
