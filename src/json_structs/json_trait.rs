pub trait JsonSerializable {
    fn to_json(&self) -> String;
}
