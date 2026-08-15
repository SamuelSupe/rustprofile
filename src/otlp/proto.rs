use prost::Message;

// Wire-compatible subset of opentelemetry-proto v1.11.0. Only fields emitted or
// consumed by rustprofile are represented; unknown fields remain protobuf-compatible.

#[derive(Clone, PartialEq, Message)]
pub struct AnyValue {
    #[prost(oneof = "any_value::Value", tags = "1, 2, 3, 4, 5, 6, 7")]
    pub value: Option<any_value::Value>,
}

pub mod any_value {
    use super::{ArrayValue, KeyValueList};
    use prost::Oneof;

    #[derive(Clone, PartialEq, Oneof)]
    pub enum Value {
        #[prost(string, tag = "1")]
        StringValue(String),
        #[prost(bool, tag = "2")]
        BoolValue(bool),
        #[prost(int64, tag = "3")]
        IntValue(i64),
        #[prost(double, tag = "4")]
        DoubleValue(f64),
        #[prost(message, tag = "5")]
        ArrayValue(ArrayValue),
        #[prost(message, tag = "6")]
        KvlistValue(KeyValueList),
        #[prost(bytes, tag = "7")]
        BytesValue(Vec<u8>),
    }
}

#[derive(Clone, PartialEq, Message)]
pub struct ArrayValue {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<AnyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyValueList {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<KeyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
}

#[derive(Clone, PartialEq, Message)]
pub struct InstrumentationScope {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub version: String,
    #[prost(message, repeated, tag = "3")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "4")]
    pub dropped_attributes_count: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Resource {
    #[prost(message, repeated, tag = "1")]
    pub attributes: Vec<KeyValue>,
    #[prost(uint32, tag = "2")]
    pub dropped_attributes_count: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExportProfilesServiceRequest {
    #[prost(message, repeated, tag = "1")]
    pub resource_profiles: Vec<ResourceProfiles>,
    #[prost(message, optional, tag = "2")]
    pub dictionary: Option<ProfilesDictionary>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExportProfilesServiceResponse {
    #[prost(message, optional, tag = "1")]
    pub partial_success: Option<ExportProfilesPartialSuccess>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ExportProfilesPartialSuccess {
    #[prost(int64, tag = "1")]
    pub rejected_profiles: i64,
    #[prost(string, tag = "2")]
    pub error_message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ResourceProfiles {
    #[prost(message, optional, tag = "1")]
    pub resource: Option<Resource>,
    #[prost(message, repeated, tag = "2")]
    pub scope_profiles: Vec<ScopeProfiles>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct ScopeProfiles {
    #[prost(message, optional, tag = "1")]
    pub scope: Option<InstrumentationScope>,
    #[prost(message, repeated, tag = "2")]
    pub profiles: Vec<Profile>,
    #[prost(string, tag = "3")]
    pub schema_url: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct Profile {
    #[prost(message, optional, tag = "1")]
    pub sample_type: Option<ValueType>,
    #[prost(message, repeated, tag = "2")]
    pub samples: Vec<Sample>,
    #[prost(fixed64, tag = "3")]
    pub time_unix_nano: u64,
    #[prost(uint64, tag = "4")]
    pub duration_nano: u64,
    #[prost(message, optional, tag = "5")]
    pub period_type: Option<ValueType>,
    #[prost(int64, tag = "6")]
    pub period: i64,
    #[prost(bytes, tag = "7")]
    pub profile_id: Vec<u8>,
    #[prost(uint32, tag = "8")]
    pub dropped_attributes_count: u32,
    #[prost(string, tag = "9")]
    pub original_payload_format: String,
    #[prost(bytes, tag = "10")]
    pub original_payload: Vec<u8>,
    #[prost(int32, repeated, packed = "true", tag = "11")]
    pub attribute_indices: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ProfilesDictionary {
    #[prost(message, repeated, tag = "1")]
    pub mapping_table: Vec<Mapping>,
    #[prost(message, repeated, tag = "2")]
    pub location_table: Vec<Location>,
    #[prost(message, repeated, tag = "3")]
    pub function_table: Vec<Function>,
    #[prost(message, repeated, tag = "4")]
    pub link_table: Vec<Link>,
    #[prost(string, repeated, tag = "5")]
    pub string_table: Vec<String>,
    #[prost(message, repeated, tag = "6")]
    pub attribute_table: Vec<KeyValueAndUnit>,
    #[prost(message, repeated, tag = "7")]
    pub stack_table: Vec<Stack>,
}

#[derive(Clone, PartialEq, Message)]
pub struct ValueType {
    #[prost(int32, tag = "1")]
    pub type_strindex: i32,
    #[prost(int32, tag = "2")]
    pub unit_strindex: i32,
}

#[derive(Clone, PartialEq, Message)]
pub struct Sample {
    #[prost(int32, tag = "1")]
    pub stack_index: i32,
    #[prost(int32, repeated, packed = "true", tag = "2")]
    pub attribute_indices: Vec<i32>,
    #[prost(int32, tag = "3")]
    pub link_index: i32,
    #[prost(int64, repeated, packed = "true", tag = "4")]
    pub values: Vec<i64>,
    #[prost(fixed64, repeated, packed = "true", tag = "5")]
    pub timestamps_unix_nano: Vec<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Mapping {
    #[prost(uint64, tag = "1")]
    pub memory_start: u64,
    #[prost(uint64, tag = "2")]
    pub memory_limit: u64,
    #[prost(uint64, tag = "3")]
    pub file_offset: u64,
    #[prost(int32, tag = "4")]
    pub filename_strindex: i32,
    #[prost(int32, repeated, packed = "true", tag = "5")]
    pub attribute_indices: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Stack {
    #[prost(int32, repeated, packed = "true", tag = "1")]
    pub location_indices: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Location {
    #[prost(int32, tag = "1")]
    pub mapping_index: i32,
    #[prost(uint64, tag = "2")]
    pub address: u64,
    #[prost(message, repeated, tag = "3")]
    pub lines: Vec<Line>,
    #[prost(int32, repeated, packed = "true", tag = "4")]
    pub attribute_indices: Vec<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct Line {
    #[prost(int32, tag = "1")]
    pub function_index: i32,
    #[prost(int64, tag = "2")]
    pub line: i64,
    #[prost(int64, tag = "3")]
    pub column: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Function {
    #[prost(int32, tag = "1")]
    pub name_strindex: i32,
    #[prost(int32, tag = "2")]
    pub system_name_strindex: i32,
    #[prost(int32, tag = "3")]
    pub filename_strindex: i32,
    #[prost(int64, tag = "4")]
    pub start_line: i64,
}

#[derive(Clone, PartialEq, Message)]
pub struct Link {
    #[prost(bytes, tag = "1")]
    pub trace_id: Vec<u8>,
    #[prost(bytes, tag = "2")]
    pub span_id: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct KeyValueAndUnit {
    #[prost(int32, tag = "1")]
    pub key_strindex: i32,
    #[prost(message, optional, tag = "2")]
    pub value: Option<AnyValue>,
    #[prost(int32, tag = "3")]
    pub unit_strindex: i32,
}
