//! Sample household mail that seeds a dev database, standing in for JMAP
//! until import exists. Names and messages follow the wireframes: Alex and
//! Sam share the household account, and Eli's account is monitored read-only.

use std::collections::BTreeSet;

use jiff::civil::{DateTime, date};

use crate::Error;
use crate::model::{Account, Comment, Kind, Message, State, Thread, User, Values, reverse_hex};
use crate::store::{Clock, Store};

/// Seeds for the rows tests name by number.
pub const WATER: u32 = 7;
pub const ELI_PRACTICE: u32 = 20;

pub const ALEX: &str = "alex@example.com";
pub const SAM: &str = "sam@example.com";

/// The fixture row id for a seed number: the seed hashed (splitmix64)
/// and rendered as jj-style reverse hex, so rows keep stable jj-looking
/// ids while the fixtures stay written in numbers.
pub fn id(seed: u32) -> String {
    reverse_hex(splitmix64(u64::from(seed)))
}

/// The fixture Message-ID for a seed number.
pub fn message_id(seed: u32) -> String {
    format!("{}@fixtures.docket.invalid", id(seed))
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Thursday, 1 October 2026, at noon.
pub fn now() -> DateTime {
    at(10, 1, 12, 0)
}

fn at(month: i8, day: i8, hour: i8, minute: i8) -> DateTime {
    date(2026, month, day).at(hour, minute, 0, 0)
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|&x| x.to_owned()).collect()
}

#[derive(Default)]
struct Builder {
    threads: Vec<Thread>,
    messages: Vec<Message>,
    comments: Vec<Comment>,
}

impl Builder {
    fn thread(&mut self, seed: u32, account: &str, subject: &str) {
        self.threads.push(Thread {
            id: id(seed),
            account: account.to_owned(),
            subject: subject.to_owned(),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn recv(
        &mut self,
        seed: u32,
        thread: u32,
        at: DateTime,
        (from, addr): (&str, &str),
        body: &str,
        (state, folder, assignees): (State, Option<&str>, &[&str]),
        cc: &[&str],
    ) {
        self.messages.push(Message {
            id: id(seed),
            message_id: message_id(seed),
            thread: id(thread),
            at,
            cc: strings(cc),
            bcc: Vec::new(),
            body: body.to_owned(),
            kind: Kind::Received {
                from: from.to_owned(),
                addr: addr.to_owned(),
                values: Values {
                    state,
                    folder: folder.map(str::to_owned),
                    assignees: assignees
                        .iter()
                        .map(|&a| a.to_owned())
                        .collect::<BTreeSet<_>>(),
                },
            },
        });
    }

    fn sent(
        &mut self,
        seed: u32,
        thread: u32,
        at: DateTime,
        (by, to): (&str, &[&str]),
        body: &str,
        (cc, bcc): (&[&str], &[&str]),
    ) {
        self.messages.push(Message {
            id: id(seed),
            message_id: message_id(seed),
            thread: id(thread),
            at,
            cc: strings(cc),
            bcc: strings(bcc),
            body: body.to_owned(),
            kind: Kind::Sent {
                by: Some(by.to_owned()),
                to: strings(to),
            },
        });
    }

    fn comment(&mut self, thread: u32, author: &str, at: DateTime, text: &str) {
        let n = u32::try_from(self.comments.len())
            .unwrap_or(u32::MAX)
            .saturating_add(1);
        self.comments.push(Comment {
            id: id(n),
            thread: id(thread),
            author: author.to_owned(),
            at,
            text: text.to_owned(),
        });
    }
}

/// A fresh in-memory store holding the fixtures.
pub fn store() -> Result<Store, Error> {
    let store = Store::open_in_memory(Clock::Fixed(now()))?;
    seed(&store)?;
    Ok(store)
}

/// Writes the fixtures into an empty database.
pub fn seed(store: &Store) -> Result<(), Error> {
    let users = [User::new(ALEX, "Alex"), User::new(SAM, "Sam")];
    let accounts = [
        Account {
            slug: "household".into(),
            name: "Household".into(),
            address: "household@example.com".into(),
            read_only: false,
        },
        Account {
            slug: "eli".into(),
            name: "Eli".into(),
            address: "eli@example.com".into(),
            read_only: true,
        },
    ];
    let folders = ["Medical", "School", "House", "Finance"];
    let mut b = Builder::default();

    use State::{Do, Done, Inbox, Wait, Watch};
    let roofer = ("Northwind Roofing", "office@northwind.co");
    let school = ("Lincoln High School", "office@lincolnhigh.org");

    b.thread(1, "household", "Gutter repair estimate");
    b.recv(
        1,
        1,
        at(9, 20, 9, 5),
        roofer,
        "Estimate for gutter repair attached: $1,840, two days.",
        (Done, Some("House"), &[]),
        &[],
    );
    b.comment(
        1,
        ALEX,
        at(9, 20, 19, 30),
        "The Hendersons paid about $1,600 for theirs.",
    );
    b.sent(
        2,
        1,
        at(9, 21, 8, 40),
        (SAM, &["Northwind Roofing"]),
        "Does this include the downspout on the north side?",
        (&["Alex"], &["Pat Lee"]),
    );
    b.recv(
        3,
        1,
        at(9, 26, 14, 10),
        roofer,
        "Here are the photos from the site visit.",
        (Inbox, Some("House"), &[]),
        &[],
    );
    b.recv(
        4,
        1,
        at(9, 27, 10, 2),
        roofer,
        "Revised estimate attached, includes the downspout: $2,120. We can start October 14 if you confirm by the 3rd.",
        (Inbox, Some("House"), &[]),
        &["Sam", "Alex"],
    );

    b.thread(2, "household", "Field trip permission form");
    b.recv(
        5,
        2,
        at(9, 30, 15, 30),
        school,
        "Please return the signed permission form by Friday.",
        (Do, Some("School"), &[SAM]),
        &[],
    );
    b.recv(
        6,
        2,
        at(10, 1, 9, 12),
        school,
        "A schedule update for the museum trip: buses now return at 4:15, so pickup moves to 4:30 PM at the front entrance.",
        (Inbox, Some("School"), &[ALEX]),
        &[],
    );
    b.comment(2, SAM, at(10, 1, 9, 20), "You have pickup that day.");

    b.thread(3, "household", "Service interruption Oct 4");
    b.recv(
        WATER,
        3,
        at(10, 1, 11, 20),
        ("City Water", "notices@citywater.gov"),
        "Crews will be working on Elm St between 9 AM and 1 PM. Expect low pressure until the afternoon.",
        (Inbox, None, &[]),
        &[],
    );

    b.thread(4, "household", "Your September bill is ready");
    b.recv(
        8,
        4,
        at(9, 30, 7, 45),
        ("PG&E", "billing@pge.com"),
        "Amount due $142.18 by October 21.",
        (Inbox, None, &[]),
        &[],
    );

    b.thread(5, "household", "Your order has shipped");
    b.recv(
        9,
        5,
        at(9, 28, 16, 0),
        ("Costco", "orders@costco.com"),
        "Estimated delivery Wednesday.",
        (Inbox, None, &[]),
        &[],
    );

    b.thread(6, "household", "Eli: appointment confirmed");
    b.recv(
        10,
        6,
        at(10, 1, 8, 15),
        ("Dr. Patel’s Office", "frontdesk@patelpeds.com"),
        "This confirms Eli’s appointment on Thursday, October 8 at 3:00 PM. Please arrive ten minutes early.",
        (Inbox, Some("Medical"), &[ALEX]),
        &[],
    );
    b.comment(6, SAM, at(10, 1, 9, 41), "Thursday at 3, I’ll take him.");

    b.thread(7, "household", "Claim 4471: adjuster visit");
    b.recv(
        11,
        7,
        at(9, 23, 13, 0),
        ("State Farm", "claims@statefarm.com"),
        "Claim 4471 is open. An adjuster will contact you within five business days to schedule a visit.",
        (Wait, Some("House"), &[ALEX]),
        &[],
    );
    b.sent(
        12,
        7,
        at(9, 24, 9, 30),
        (ALEX, &["State Farm"]),
        "Thanks. Mornings work best for us, any day next week.",
        (&[], &[]),
    );

    b.thread(8, "household", "Exemption renewal");
    b.recv(
        13,
        8,
        at(9, 25, 10, 0),
        ("County Assessor", "assessor@county.gov"),
        "Your homeowner’s exemption must be renewed by October 31. The form is attached.",
        (Do, Some("Finance"), &[]),
        &[],
    );

    b.thread(9, "household", "Shipped: furnace filters");
    b.recv(
        14,
        9,
        at(9, 30, 18, 5),
        ("Amazon", "shipment-tracking@amazon.com"),
        "Your package will arrive Friday.",
        (Watch, Some("House"), &[]),
        &[],
    );

    b.thread(10, "household", "Your annual checkup");
    b.recv(
        15,
        10,
        at(9, 10, 11, 0),
        ("Dr. Patel’s Office", "frontdesk@patelpeds.com"),
        "Thanks for visiting. Your visit summary is available in the portal.",
        (Done, Some("Medical"), &[]),
        &[],
    );

    b.thread(11, "eli", "Practice moved to Thursday");
    b.recv(
        ELI_PRACTICE,
        11,
        at(9, 29, 17, 0),
        ("Coach Rivera", "rivera@lincolnhigh.org"),
        "Practice moves to Thursday this week, 4 to 6 PM. Bring water.",
        (Watch, None, &[SAM]),
        &[],
    );

    // Everything before yesterday has been read by both of us.
    let cutoff = at(9, 30, 0, 0);
    let mut reads: Vec<_> = b
        .messages
        .iter()
        .filter(|m| m.at < cutoff)
        .flat_map(|m| [(ALEX, m.id.clone()), (SAM, m.id.clone())])
        .collect();
    reads.push((SAM, id(5)));

    store.import(|tx| {
        for user in &users {
            tx.user(user)?;
        }
        for account in &accounts {
            tx.account(account)?;
        }
        for folder in folders {
            tx.folder(folder)?;
        }
        for thread in &b.threads {
            tx.thread(thread)?;
        }
        for message in &b.messages {
            tx.message(message)?;
        }
        for comment in &b.comments {
            tx.comment(comment)?;
        }
        for (user, id) in reads {
            tx.read(user, &id)?;
        }
        Ok(())
    })
}
