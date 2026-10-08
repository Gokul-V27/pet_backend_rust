//! Double-entry bookkeeping. Every money event becomes one balanced journal entry; reports read the
//! ledger, never ad-hoc sums. The posting rules are plain functions (easy to test); `post` writes
//! them inside the caller's transaction. The database also refuses unbalanced or edited entries.
//!
//! | Event | Debit | Credit |
//! |---|---|---|
//! | Online payment captured | 1010 Gateway clearing | 2400 Customer advances |
//! | Order delivered | 2400 (paid online) or 1000 Cash (COD) | 4000 Sales, 4100 Delivery, 4200 COD fee, 2100 GST, 2200 Donations |
//! | Refund before delivery | 2400 Customer advances | 1010 / 1000 |
//! | Refund after delivery | 4500 Sales returns + 2100 GST | 1010 / 1000 |
//!
//! Revenue is recognised on delivery: money paid before that is a liability (customer advances).

use sqlx::PgConnection;
use uuid::Uuid;

use crate::error::{AppError, Result};
use crate::services::pricing::{FEE_GST_RATE_PCT, gst_inside};

pub const CASH: &str = "1000";
pub const GATEWAY: &str = "1010";
pub const GST_PAYABLE: &str = "2100";
pub const DONATIONS: &str = "2200";
pub const ADVANCES: &str = "2400";
pub const SALES: &str = "4000";
pub const DELIVERY_INCOME: &str = "4100";
pub const COD_INCOME: &str = "4200";
pub const SALES_RETURNS: &str = "4500";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub account: &'static str,
    pub debit: i64,
    pub credit: i64,
}

const fn dr(account: &'static str, paise: i64) -> Line {
    Line {
        account,
        debit: paise,
        credit: 0,
    }
}
const fn cr(account: &'static str, paise: i64) -> Line {
    Line {
        account,
        debit: 0,
        credit: paise,
    }
}

/// The money facts of one order, in paise.
#[derive(Debug, Clone)]
pub struct OrderAmounts {
    pub total: i64,
    /// Per line: (what the customer pays after the coupon share, GST inside that).
    pub lines: Vec<(i64, i64)>,
    pub delivery: i64,
    pub cod_fee: i64,
    pub donation: i64,
    pub gst: i64,
    pub paid_online: bool,
}

/// Drops zero lines and checks the entry balances.
fn finish(lines: Vec<Line>) -> Result<Vec<Line>> {
    let lines: Vec<Line> = lines
        .into_iter()
        .filter(|l| l.debit > 0 || l.credit > 0)
        .collect();
    let debits: i64 = lines.iter().map(|l| l.debit).sum();
    let credits: i64 = lines.iter().map(|l| l.credit).sum();
    if lines.is_empty() || debits != credits || lines.iter().any(|l| l.debit < 0 || l.credit < 0) {
        return Err(AppError::Other(anyhow::anyhow!(
            "unbalanced journal entry: debits {debits}, credits {credits}"
        )));
    }
    Ok(lines)
}

pub fn payment_captured(amount: i64) -> Result<Vec<Line>> {
    finish(vec![dr(GATEWAY, amount), cr(ADVANCES, amount)])
}

/// Revenue, fees, GST and the donation the moment the order is delivered.
pub fn delivered(o: &OrderAmounts) -> Result<Vec<Line>> {
    let sales: i64 = o.lines.iter().map(|(net, gst)| net - gst).sum();
    let line_gst: i64 = o.lines.iter().map(|(_, gst)| gst).sum();
    let delivery_gst = gst_inside(o.delivery, FEE_GST_RATE_PCT);
    let cod_gst = gst_inside(o.cod_fee, FEE_GST_RATE_PCT);
    finish(vec![
        dr(if o.paid_online { ADVANCES } else { CASH }, o.total),
        cr(SALES, sales),
        cr(DELIVERY_INCOME, o.delivery - delivery_gst),
        cr(COD_INCOME, o.cod_fee - cod_gst),
        cr(GST_PAYABLE, line_gst + delivery_gst + cod_gst),
        cr(DONATIONS, o.donation),
    ])
}

/// Money going back. Before delivery (a cancelled order) it only reverses the advance; after
/// delivery it's a sales return, with the GST and the donation (each in the order's own share of the
/// total) taken back off GST payable and the charity liability.
///
/// `already_refunded` is what was refunded on this order before, in paise. Each share is the
/// difference of two running figures, so any number of partial refunds adds up to exactly the
/// order's GST and donation (no stray paisa left in the books after a full refund).
pub fn refund(
    amount: i64,
    already_refunded: i64,
    o: &OrderAmounts,
    after_delivery: bool,
) -> Result<Vec<Line>> {
    let paid_from = if o.paid_online { GATEWAY } else { CASH };
    if !after_delivery {
        return finish(vec![dr(ADVANCES, amount), cr(paid_from, amount)]);
    }
    let part = |of: i64, x: i64| {
        if o.total > 0 {
            (x * of + o.total / 2) / o.total
        } else {
            0
        }
    };
    let gst_share =
        (part(o.gst, already_refunded + amount) - part(o.gst, already_refunded)).clamp(0, amount);
    let donation_share = (part(o.donation, already_refunded + amount)
        - part(o.donation, already_refunded))
    .clamp(0, amount - gst_share);
    finish(vec![
        dr(SALES_RETURNS, amount - gst_share - donation_share),
        dr(GST_PAYABLE, gst_share),
        dr(DONATIONS, donation_share),
        cr(paid_from, amount),
    ])
}

/// The mirror image of a posted entry: every debit becomes a credit and the other way round.
pub fn reversed(lines: &[Line]) -> Vec<Line> {
    lines
        .iter()
        .map(|l| Line {
            account: l.account,
            debit: l.credit,
            credit: l.debit,
        })
        .collect()
}

/// Reads back a posted entry's lines (for reversing it).
pub async fn lines_of(
    conn: &mut PgConnection,
    source_type: &str,
    source_id: Uuid,
    purpose: &str,
) -> Result<Vec<Line>> {
    let rows: Vec<(String, i64, i64)> = sqlx::query_as(
        "SELECT l.account_code, l.debit_paise, l.credit_paise
           FROM journal_lines l JOIN journal_entries e ON e.id = l.entry_id
          WHERE e.source_type = $1 AND e.source_id = $2 AND e.purpose = $3 ORDER BY l.id",
    )
    .bind(source_type)
    .bind(source_id)
    .bind(purpose)
    .fetch_all(&mut *conn)
    .await?;
    rows.into_iter()
        .map(|(code, debit, credit)| {
            let account = [
                CASH,
                GATEWAY,
                GST_PAYABLE,
                DONATIONS,
                ADVANCES,
                SALES,
                DELIVERY_INCOME,
                COD_INCOME,
                SALES_RETURNS,
            ]
            .into_iter()
            .find(|a| *a == code)
            .ok_or_else(|| AppError::Other(anyhow::anyhow!("unknown ledger account {code}")))?;
            Ok(Line {
                account,
                debit,
                credit,
            })
        })
        .collect()
}

/// Writes a journal entry inside the caller's transaction. Posting the same
/// (source_type, source_id, purpose) twice is a no-op, so retries and replayed webhooks are safe.
/// Returns the new entry's id, or `None` if it was already posted.
pub async fn post(
    conn: &mut PgConnection,
    source_type: &str,
    source_id: Uuid,
    purpose: &str,
    memo: &str,
    created_by: &str,
    lines: &[Line],
) -> Result<Option<Uuid>> {
    let entry: Option<(Uuid,)> = sqlx::query_as(
        "INSERT INTO journal_entries (memo, source_type, source_id, purpose, created_by)
         VALUES ($1, $2, $3, $4, $5)
         ON CONFLICT (source_type, source_id, purpose) DO NOTHING
         RETURNING id",
    )
    .bind(memo)
    .bind(source_type)
    .bind(source_id)
    .bind(purpose)
    .bind(created_by)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((entry_id,)) = entry else {
        return Ok(None);
    };
    for l in lines {
        sqlx::query(
            "INSERT INTO journal_lines (entry_id, account_code, debit_paise, credit_paise) VALUES ($1, $2, $3, $4)",
        )
        .bind(entry_id)
        .bind(l.account)
        .bind(l.debit)
        .bind(l.credit)
        .execute(&mut *conn)
        .await?;
    }
    Ok(Some(entry_id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn balanced(lines: &[Line]) -> bool {
        lines.iter().map(|l| l.debit).sum::<i64>() == lines.iter().map(|l| l.credit).sum::<i64>()
    }

    fn order(paid_online: bool) -> OrderAmounts {
        // ₹949 food (GST 14476 paise inside), ₹49 delivery, ₹29 COD, ₹5 donation.
        let lines = vec![(94_900, 14_476)];
        let (delivery, cod_fee, donation) = (4_900, if paid_online { 0 } else { 2_900 }, 500);
        OrderAmounts {
            total: 94_900 + delivery + cod_fee + donation,
            gst: 14_476 + gst_inside(delivery, 18) + gst_inside(cod_fee, 18),
            lines,
            delivery,
            cod_fee,
            donation,
            paid_online,
        }
    }

    #[test]
    fn capture_moves_money_into_advances() {
        let l = payment_captured(1_000).unwrap();
        assert_eq!(l, vec![dr(GATEWAY, 1_000), cr(ADVANCES, 1_000)]);
    }

    #[test]
    fn delivery_recognises_revenue_and_balances() {
        for online in [true, false] {
            let o = order(online);
            let l = delivered(&o).unwrap();
            assert!(balanced(&l));
            assert_eq!(l[0], dr(if online { ADVANCES } else { CASH }, o.total));
            let gst: i64 = l
                .iter()
                .filter(|x| x.account == GST_PAYABLE)
                .map(|x| x.credit)
                .sum();
            assert_eq!(gst, o.gst);
            assert!(l.iter().any(|x| x == &cr(DONATIONS, 500)));
            // No zero lines (no COD fee line when paid online).
            assert!(l.iter().all(|x| x.debit > 0 || x.credit > 0));
        }
    }

    #[test]
    fn refunds_balance_before_and_after_delivery() {
        let o = order(true);
        let before = refund(o.total, 0, &o, false).unwrap();
        assert_eq!(before, vec![dr(ADVANCES, o.total), cr(GATEWAY, o.total)]);
        let after = refund(50_000, 0, &o, true).unwrap();
        assert!(balanced(&after));
        assert!(
            after
                .iter()
                .any(|x| x.account == GST_PAYABLE && x.debit > 0)
        );
    }

    /// Any way of splitting refunds adds back to exactly the order's GST and donation, so a full
    /// refund leaves nothing behind in GST payable or the charity liability.
    #[test]
    fn partial_refunds_add_up_exactly() {
        for online in [true, false] {
            let o = order(online);
            for split in [
                vec![333, 333, 0],
                vec![1, 99_999, 0],
                vec![50_000, 49_999, 0],
                vec![7, 13, 0],
            ] {
                let mut parts = split.clone();
                let given: i64 = parts.iter().sum();
                *parts.last_mut().unwrap() = o.total - given; // the rest
                let (mut gst, mut donation, mut returns, mut done) = (0, 0, 0, 0);
                for amount in parts {
                    if amount == 0 {
                        continue;
                    }
                    let l = refund(amount, done, &o, true).unwrap();
                    assert!(balanced(&l));
                    let sum = |acc: &str| {
                        l.iter()
                            .filter(|x| x.account == acc)
                            .map(|x| x.debit)
                            .sum::<i64>()
                    };
                    gst += sum(GST_PAYABLE);
                    donation += sum(DONATIONS);
                    returns += sum(SALES_RETURNS);
                    done += amount;
                }
                assert_eq!(gst, o.gst, "GST fully reversed");
                assert_eq!(donation, o.donation, "donation fully reversed");
                assert_eq!(gst + donation + returns, o.total);
            }
        }
    }

    #[test]
    fn a_reversal_swaps_debits_and_credits() {
        let l = refund(50_000, 0, &order(true), true).unwrap();
        let r = reversed(&l);
        assert!(balanced(&r));
        for (a, b) in l.iter().zip(&r) {
            assert_eq!((a.debit, a.credit), (b.credit, b.debit));
        }
    }

    #[test]
    fn empty_or_unbalanced_entries_are_refused() {
        assert!(finish(vec![]).is_err());
        assert!(finish(vec![dr(CASH, 10), cr(SALES, 9)]).is_err());
        assert!(payment_captured(0).is_err());
    }
}
