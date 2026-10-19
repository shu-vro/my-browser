//! Autofill for addresses and cards (Settings → Autofill).
//!
//! Addresses and a card's name and expiry are kept in the profile's database;
//! card numbers in the keychain (passwords.rs), behind Touch ID like
//! passwords. resources/passwords.js, already in every frame, also reports
//! focus on address and card fields; the browser shows the saved entries in
//! a native menu under the field, and fills the form only with the one the
//! user picks. Cards are filled only on HTTPS pages (or this Mac's), and the
//! security code is never kept.

use crate::App;
use crate::store::{Autofill, json_str};

/// Settings → Autofill's list: `[{"id","kind","label","fields":{…}}]`. A
/// card's number never leaves the keychain: only its last four digits show.
pub fn list_json(app: &App) -> String {
    let items: Vec<String> = app
        .store()
        .autofill(None)
        .iter()
        .map(|e| {
            let fields: Vec<String> = e
                .fields
                .iter()
                .filter(|(k, _)| k != "ref")
                .map(|(k, v)| format!("{}:{}", json_str(k), json_str(v)))
                .collect();
            format!(
                "{{\"id\":{},\"kind\":{},\"label\":{},\"fields\":{{{}}}}}",
                e.id,
                json_str(&e.kind),
                json_str(&label(e)),
                fields.join(",")
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

/// What the fill menu shows for an entry.
pub fn label(e: &Autofill) -> String {
    if e.kind == "card" {
        let name = e.get("name");
        let card = format!("{} •••• {}", e.get("brand"), e.get("last4"));
        return if name.is_empty() { card } else { format!("{card} ({name})") };
    }
    let parts: Vec<&str> =
        ["name", "street", "city", "email"].iter().map(|f| e.get(f)).filter(|v| !v.is_empty()).take(3).collect();
    if parts.is_empty() { "Address".into() } else { parts.join(", ") }
}

/// Add or change an entry from the settings page: `fields` are (name,
/// value) pairs. A card's `number` goes to the keychain (left empty when
/// editing, the saved one stays). Returns the entry's id.
pub fn save(app: &App, id: i64, kind: &str, fields: &[(String, String)]) -> Result<i64, String> {
    let old = (id != 0).then(|| app.store().autofill(Some(kind)).into_iter().find(|e| e.id == id)).flatten();
    if id != 0 && old.is_none() {
        return Err("no such entry".into());
    }
    if kind != "card" {
        return app.store().save_autofill(id, kind, fields);
    }
    let get = |k: &str| fields.iter().find(|(f, _)| f == k).map_or("", |(_, v)| v.trim());
    let (month, year) = (get("month"), get("year"));
    if !month.is_empty() && !month.parse::<u32>().is_ok_and(|m| (1..=12).contains(&m)) {
        return Err("The month is a number from 1 to 12.".into());
    }
    if !year.is_empty() && !(year.len() == 4 && year.parse::<u32>().is_ok()) {
        return Err("The year has four digits.".into());
    }
    let mut keep: Vec<(String, String)> = [("name", get("name")), ("month", month), ("year", year)]
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let number = get("number");
    let (brand, last4, card_ref) = if number.is_empty() {
        let old = old.ok_or("Enter the card number.")?;
        (old.get("brand").to_string(), old.get("last4").to_string(), old.get("ref").to_string())
    } else {
        let digits = crate::passwords::card_digits(number).ok_or("That isn't a valid card number.")?;
        let card_ref = old.as_ref().map(|o| o.get("ref").to_string()).filter(|r| !r.is_empty());
        let card_ref = card_ref.unwrap_or_else(|| objc2_foundation::NSUUID::UUID().UUIDString().to_string());
        app.vault().save_card_number(&card_ref, &digits)?;
        (crate::passwords::card_brand(&digits).to_string(), digits[digits.len() - 4..].to_string(), card_ref)
    };
    keep.extend([("brand".into(), brand), ("last4".into(), last4), ("ref".into(), card_ref)]);
    app.store().save_autofill(id, kind, &keep)
}

pub fn delete(app: &App, id: i64) {
    if let Some(card) = app.store().autofill(Some("card")).into_iter().find(|e| e.id == id) {
        app.vault().delete_card_number(card.get("ref"));
    }
    app.store().delete_autofill(id);
}

/// What resources/passwords.js fills, by the field names it recognises
/// (`autocomplete` tokens). A card's number is read from the keychain here.
pub fn values(app: &App, e: &Autofill) -> Vec<(&'static str, String)> {
    if e.kind == "card" {
        let (month, year) = (e.get("month"), e.get("year"));
        let mm = if month.len() == 1 { format!("0{month}") } else { month.to_string() };
        let yy = year.get(2..).unwrap_or("").to_string();
        let number = app.vault().card_number(e.get("ref")).unwrap_or_default();
        let (given, family) = split_name(e.get("name"));
        return vec![
            ("cc-name", e.get("name").into()),
            ("cc-given-name", given),
            ("cc-family-name", family),
            ("cc-number", number),
            ("cc-exp-month", mm.clone()),
            ("cc-exp-year", year.into()),
            ("cc-exp", if mm.is_empty() || yy.is_empty() { String::new() } else { format!("{mm}/{yy}") }),
        ];
    }
    let street = e.get("street");
    let (line1, line2) = street.split_once(", ").unwrap_or((street, ""));
    let (given, family) = split_name(e.get("name"));
    vec![
        ("name", e.get("name").into()),
        ("given-name", given),
        ("family-name", family),
        ("organization", e.get("organization").into()),
        ("street-address", street.into()),
        ("address-line1", line1.into()),
        ("address-line2", line2.into()),
        ("address-level2", e.get("city").into()),
        ("address-level1", e.get("region").into()),
        ("postal-code", e.get("postal").into()),
        ("country", e.get("country").into()),
        ("country-name", e.get("country").into()),
        ("email", e.get("email").into()),
        ("tel", e.get("tel").into()),
    ]
}

/// "Ada King Lovelace" → ("Ada", "King Lovelace").
fn split_name(name: &str) -> (String, String) {
    let name = name.trim();
    let (given, family) = name.split_once(' ').unwrap_or((name, ""));
    (given.to_string(), family.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_and_names() {
        let e = |kind: &str, f: &[(&str, &str)]| Autofill {
            id: 1,
            kind: kind.into(),
            fields: f.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
        };
        assert_eq!(label(&e("card", &[("brand", "Visa"), ("last4", "4242"), ("name", "Ada")])), "Visa •••• 4242 (Ada)");
        assert_eq!(label(&e("address", &[("name", "Ada"), ("city", "London"), ("tel", "1")])), "Ada, London");
        assert_eq!(label(&e("address", &[])), "Address");
        assert_eq!(split_name("Ada King Lovelace"), ("Ada".into(), "King Lovelace".into()));
        assert_eq!(split_name("Cher"), ("Cher".into(), String::new()));
    }
}
