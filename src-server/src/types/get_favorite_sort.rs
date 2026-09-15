use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum GetFavoriteSort {
    TimeNewest,
    TimeOldest,
}

impl GetFavoriteSort {
    pub fn as_str(&self) -> &'static str {
        match self {
            GetFavoriteSort::TimeNewest => "dd",
            GetFavoriteSort::TimeOldest => "da",
        }
    }
}
