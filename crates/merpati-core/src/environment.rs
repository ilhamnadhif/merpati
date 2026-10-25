use serde::{Deserialize, Serialize};

use crate::model::Id;
use crate::util::new_id;
use crate::variables::{self, Variable};

/// A named set of variables (local, staging, production, ...). One
/// environment is active at a time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Environment {
    #[serde(default = "new_id")]
    pub id: Id,
    pub name: String,
    #[serde(default)]
    pub variables: Vec<Variable>,
}

impl Environment {
    pub fn new(name: impl Into<String>) -> Self {
        Environment { id: new_id(), name: name.into(), variables: Vec::new() }
    }

    pub fn with_var(mut self, key: &str, value: &str) -> Self {
        self.variables.push(Variable::new(key, value));
        self
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.variables.iter().rev().find(|v| v.enabled && v.key == key).map(|v| v.effective())
    }

    /// Sets the current value of `key`, adding the variable if needed.
    pub fn set_current(&mut self, key: &str, value: &str) {
        match self.variables.iter_mut().rev().find(|v| v.key == key) {
            Some(v) => v.current = Some(value.to_string()),
            None => {
                let mut v = Variable::new(key, "");
                v.current = Some(value.to_string());
                self.variables.push(v);
            }
        }
    }

    pub fn to_map(&self) -> indexmap::IndexMap<String, String> {
        variables::to_map(&self.variables)
    }

    /// Types of the values [`to_map`](Self::to_map) lists, for
    /// `VariableScopes::types.environment`; see [`variables::to_types`].
    pub fn to_types(&self) -> indexmap::IndexMap<String, variables::VarType> {
        variables::to_types(&self.variables)
    }
}
