use std::str::FromStr;

use serde::Serialize;
use sqlx::FromRow;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, FromRow)]
pub struct Customer {
    pub id: Uuid,
    pub mobile: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

/// Same five roles as the admin website's role switcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRole {
    SuperAdmin,
    Admin,
    OrderManager,
    InventoryManager,
    Support,
}

impl AdminRole {
    /// Products, categories and their pictures (same as the admin website's role switcher).
    pub fn can_manage_catalogue(self) -> bool {
        matches!(
            self,
            Self::SuperAdmin | Self::Admin | Self::InventoryManager
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::SuperAdmin => "super_admin",
            Self::Admin => "admin",
            Self::OrderManager => "order_manager",
            Self::InventoryManager => "inventory_manager",
            Self::Support => "support",
        }
    }
}

impl FromStr for AdminRole {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        Ok(match s {
            "super_admin" => Self::SuperAdmin,
            "admin" => Self::Admin,
            "order_manager" => Self::OrderManager,
            "inventory_manager" => Self::InventoryManager,
            "support" => Self::Support,
            _ => return Err(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip() {
        for r in [
            AdminRole::SuperAdmin,
            AdminRole::Admin,
            AdminRole::OrderManager,
            AdminRole::InventoryManager,
            AdminRole::Support,
        ] {
            assert_eq!(r.as_str().parse::<AdminRole>(), Ok(r));
        }
        assert!("root".parse::<AdminRole>().is_err());
    }
}
