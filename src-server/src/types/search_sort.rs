use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SearchSort {
    TimeNewest,
    TimeOldest,
    LikeMost,
    ViewMost,
}

impl SearchSort {
    pub fn as_str(&self) -> &'static str {
        match self {
            SearchSort::TimeNewest => "dd",
            SearchSort::TimeOldest => "da",
            SearchSort::LikeMost => "ld",
            SearchSort::ViewMost => "vd",
        }
    }
}
