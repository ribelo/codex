pub(crate) fn canonical_model_id(provider_id: &str, model: &str) -> String {
    format!("{provider_id}/{model}")
}
