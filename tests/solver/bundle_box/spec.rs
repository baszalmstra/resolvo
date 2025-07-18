use super::Pack;
use chumsky::{Parser, error};
use version_ranges::Ranges;

/// We can use this to see if a `Pack` is contained in a range of package
/// versions or a `Spec`
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Spec {
    pub name: String,
    pub versions: Ranges<Pack>,
    pub extra: Option<String>,
}

impl Spec {
    pub fn new(name: String, versions: Ranges<Pack>) -> Self {
        Self { name, versions, extra: None }
    }

    pub fn new_with_extra(name: String, extra: String, versions: Ranges<Pack>) -> Self {
        Self { name, versions, extra: Some(extra) }
    }
}

impl Spec {
    pub fn from_str(s: &str) -> Result<Self, Vec<error::Simple<'_, char>>> {
        super::parser::spec().parse(s).into_result()
    }
}
