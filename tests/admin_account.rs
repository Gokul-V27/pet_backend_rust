//! Staff accounts: the rules `create-admin` enforces (email, name, password, role), the role
//! permissions, and the `create-admin.sh` helper refusing bad input before it touches a database.

use std::io::Write;
use std::process::{Command, Stdio};
use std::str::FromStr;

use wagwell_api::models::AdminRole;
use wagwell_api::services::password::{
    MIN_LENGTH, check_strength, dummy_hash, hash, looks_like_email, verify,
};

const ROLES: [AdminRole; 5] = [
    AdminRole::SuperAdmin,
    AdminRole::Admin,
    AdminRole::OrderManager,
    AdminRole::InventoryManager,
    AdminRole::Support,
];

// ---------- password strength ----------

#[test]
fn minimum_length_is_twelve() {
    assert_eq!(MIN_LENGTH, 12);
}

#[test]
fn eleven_characters_are_refused_and_twelve_accepted() {
    assert_eq!(
        check_strength(&"a".repeat(11)),
        Err("use at least 12 characters")
    );
    assert!(check_strength(&"a".repeat(12)).is_ok());
}

#[test]
fn a_short_word_like_tester_is_refused() {
    assert!(check_strength("tester").is_err());
}

#[test]
fn empty_password_is_refused() {
    assert!(check_strength("").is_err());
}

#[test]
fn up_to_128_characters_are_allowed_and_129_refused() {
    assert!(check_strength(&"x".repeat(128)).is_ok());
    assert_eq!(
        check_strength(&"x".repeat(129)),
        Err("use at most 128 characters")
    );
}

#[test]
fn length_counts_characters_not_bytes() {
    // 11 two-byte characters = 22 bytes, still too short; 12 of them is enough.
    assert!(check_strength(&"é".repeat(11)).is_err());
    assert!(check_strength(&"é".repeat(12)).is_ok());
}

#[test]
fn spaces_count_towards_the_length() {
    assert!(check_strength("correct horse").is_ok());
}

// ---------- email ----------

#[test]
fn ordinary_emails_are_accepted() {
    for e in ["test@gmail.com", "gokul.v@wagwell.in", "a+staff@mail.co.uk"] {
        assert!(looks_like_email(e), "{e}");
    }
}

#[test]
fn placeholder_text_is_not_an_email() {
    for e in ["YOUR_EMAIL", "your-email", "you@yourmail", ""] {
        assert!(!looks_like_email(e), "{e}");
    }
}

#[test]
fn malformed_emails_are_refused() {
    for e in [
        "@gmail.com",
        "a@@gmail.com",
        "a@b@gmail.com",
        "a@.gmail.com",
        "a@gmail.com.",
        "a@gmail",
        "a b@gmail.com",
        "a@gmail.com ",
        "a@gm\tail.com",
    ] {
        assert!(!looks_like_email(e), "{e:?}");
    }
}

#[test]
fn emails_longer_than_254_characters_are_refused() {
    let at_limit = format!("{}@gmail.com", "a".repeat(254 - "@gmail.com".len()));
    assert_eq!(at_limit.len(), 254);
    assert!(looks_like_email(&at_limit));
    assert!(!looks_like_email(&format!("a{at_limit}")));
}

// ---------- hashing ----------

#[test]
fn hashes_are_argon2id_and_never_contain_the_password() {
    let h = hash("a-long-enough-pass").unwrap();
    assert!(h.starts_with("$argon2id$"), "{h}");
    assert!(!h.contains("a-long-enough-pass"));
}

#[test]
fn the_same_password_hashes_differently_each_time() {
    let a = hash("a-long-enough-pass").unwrap();
    let b = hash("a-long-enough-pass").unwrap();
    assert_ne!(a, b, "each hash needs its own random salt");
    assert!(verify("a-long-enough-pass", &a) && verify("a-long-enough-pass", &b));
}

#[test]
fn only_the_exact_password_verifies() {
    let h = hash("Correct-Horse-12").unwrap();
    assert!(verify("Correct-Horse-12", &h));
    for wrong in [
        "correct-horse-12",
        "Correct-Horse-12 ",
        "Correct-Horse-1",
        "",
    ] {
        assert!(!verify(wrong, &h), "{wrong:?}");
    }
}

#[test]
fn a_damaged_stored_hash_never_verifies() {
    assert!(!verify("anything", ""));
    assert!(!verify("anything", "plain-text-password"));
    let h = hash("a-long-enough-pass").unwrap();
    assert!(!verify("a-long-enough-pass", &h[..h.len() - 4]));
}

#[test]
fn the_timing_dummy_hash_is_stable_and_matches_no_normal_password() {
    assert_eq!(dummy_hash(), dummy_hash());
    assert!(dummy_hash().starts_with("$argon2id$"));
    for guess in ["tester", "password1234", "e2e-local-test-password"] {
        assert!(!verify(guess, dummy_hash()));
    }
}

// ---------- roles ----------

#[test]
fn every_role_round_trips_through_its_name() {
    for r in ROLES {
        assert_eq!(AdminRole::from_str(r.as_str()), Ok(r));
    }
}

#[test]
fn role_names_are_exact() {
    for bad in [
        "Super_Admin",
        "SUPER_ADMIN",
        "superadmin",
        "super admin",
        " admin",
        "owner",
        "",
    ] {
        assert!(AdminRole::from_str(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn only_super_admin_sees_the_audit_log() {
    for r in ROLES {
        assert_eq!(r.can_view_audit(), r == AdminRole::SuperAdmin, "{r:?}");
    }
}

#[test]
fn money_marketing_and_catalogue_edits_are_for_admins_only() {
    for r in ROLES {
        let admin = matches!(r, AdminRole::SuperAdmin | AdminRole::Admin);
        assert_eq!(r.can_handle_money(), admin, "{r:?} money");
        assert_eq!(r.can_run_marketing(), admin, "{r:?} marketing");
        assert_eq!(r.can_edit_catalogue(), admin, "{r:?} catalogue");
    }
}

#[test]
fn everyone_can_view_orders_but_only_order_staff_can_move_them() {
    for r in ROLES {
        assert!(r.can_view_orders(), "{r:?}");
        assert_eq!(
            r.can_manage_orders(),
            matches!(
                r,
                AdminRole::SuperAdmin | AdminRole::Admin | AdminRole::OrderManager
            ),
            "{r:?}"
        );
    }
}

#[test]
fn stock_and_catalogue_management_include_inventory_managers() {
    for r in ROLES {
        let inv = matches!(
            r,
            AdminRole::SuperAdmin | AdminRole::Admin | AdminRole::InventoryManager
        );
        assert_eq!(r.can_adjust_stock(), inv, "{r:?} stock");
        assert_eq!(r.can_manage_catalogue(), inv, "{r:?} catalogue");
    }
}

#[test]
fn support_can_edit_messages_but_not_orders_or_money() {
    let s = AdminRole::Support;
    assert!(s.can_edit_messages());
    assert!(!s.can_manage_orders() && !s.can_handle_money() && !s.can_adjust_stock());
    assert!(!AdminRole::OrderManager.can_edit_messages());
    assert!(!AdminRole::InventoryManager.can_edit_messages());
}

// ---------- create-admin.sh helper ----------

/// Runs the helper with the given answers on standard input and returns (exit code, all output).
fn run_helper(answers: &[&str]) -> (i32, String) {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/create-admin.sh");
    let mut child = Command::new("bash")
        .arg(script)
        .env_remove("DATABASE_URL")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("bash runs");
    let input = answers.iter().map(|a| format!("{a}\n")).collect::<String>();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

const DB: &str = "postgres://user:db-secret-value@db.example.com:5432/app";
const PW: &str = "a-long-enough-pass";

fn refused(answers: &[&str], why: &str) -> String {
    let (code, out) = run_helper(answers);
    assert_eq!(code, 1, "should stop: {out}");
    assert!(out.contains(why), "expected {why:?} in: {out}");
    assert!(out.contains("Nothing was created"), "{out}");
    assert!(
        !out.contains("Creating"),
        "must stop before touching the database: {out}"
    );
    out
}

#[test]
fn helper_refuses_something_that_is_not_a_database_url() {
    refused(&["not-a-url"], "doesn't look like a database URL");
}

#[test]
fn helper_refuses_placeholder_or_bad_emails() {
    refused(&[DB, "YOUR_EMAIL"], "doesn't look like an email");
    refused(&[DB, "you@yourmail"], "doesn't look like an email");
}

#[test]
fn helper_refuses_an_empty_name() {
    refused(&[DB, "test@gmail.com", ""], "Name can't be empty");
}

#[test]
fn helper_refuses_mismatched_passwords() {
    refused(
        &[DB, "test@gmail.com", "tester", PW, "a-different-pass"],
        "don't match",
    );
}

#[test]
fn helper_refuses_short_passwords_before_building_anything() {
    refused(
        &[DB, "test@gmail.com", "tester", "tester", "tester"],
        "at least 12 characters",
    );
}

#[test]
fn helper_never_prints_the_database_url_or_password() {
    for answers in [
        vec![DB, "YOUR_EMAIL"],
        vec![DB, "test@gmail.com", "tester", PW, "a-different-pass"],
    ] {
        let (_, out) = run_helper(&answers);
        assert!(!out.contains("db-secret-value"), "{out}");
        assert!(!out.contains(PW), "{out}");
    }
}
