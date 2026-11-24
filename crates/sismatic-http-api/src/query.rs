//! The `where` / `filter` query language of the read routes.
//!
//! Each parameter does one thing to a response:
//!
//! | Parameter | Acts on                         | Can change the row count |
//! |-----------|---------------------------------|--------------------------|
//! | `where`   | which rows come back            | yes                      |
//! | `filter`  | which list elements a row keeps | no                       |
//!
//! `filter` never removes a row, so it runs after paging without disturbing
//! the cursor. `where` has to run before paging, on a row's whole snapshot.
//!
//! # Clauses
//!
//! ```text
//! clause = PATH ( ":" | "!:" ) [ OP ":" ] VALUE ( "|" VALUE )*
//! PATH   = FIELD [ "." ATTRIBUTE ]
//! OP     = gt | ge | lt | le | has
//! ```
//!
//! Clauses are joined by `,`, meaning AND, so a value cannot contain `,` or
//! `|`. `|` means OR between values. `!:` negates the whole clause. With no
//! `OP`, the clause tests equality.
//!
//! A path without an attribute compares a field's single value, in the shape
//! the device answered in: `8080` matches a port, `on` a flag, `stopped` a
//! recording state. Equality is [`satisfies`], so it agrees with the group
//! drift check. `gt`/`ge`/`lt`/`le` order numbers and dotted versions segment
//! by segment, so `2.9` is less than `2.11`. `has` is a case-insensitive
//! substring test on text.
//!
//! A path with an attribute reaches into a list-valued field, one element at a
//! time: `RECORDING_EVENTS.state:archive_failed`, `ACTIVE_ALARMS.level:critical`.
//! Each attribute has a fixed type (see [`Attr::ALL`]), so a value that cannot
//! have that type is a `400`. A field name is never a `400`: the store cannot
//! tell a misspelling from a field nobody has polled.
//!
//! # Quantifiers
//!
//! - **A scalar clause** holds when the row has a read of that field and the read
//!   satisfies it. A field the device never reported satisfies no clause,
//!   negated or not.
//! - **Attribute clauses on the same list** must all hold *for the same element*.
//!   `RECORDING_EVENTS.state:archive_failed,RECORDING_EVENTS.title:Week 4`
//!   means one event that is both, not two separate events.
//! - **An element without the attribute** (an event with no `state`) satisfies no
//!   clause on it, negated or not. Neither does an element of a list that has
//!   no such attribute at all, such as an alarm's `state`.
//!
//! # `filter`, and the references `where` can make to it
//!
//! `filter` takes attribute clauses only; it keeps the elements that pass every
//! clause on their list. `where` can ask about the result:
//!
//! | In `where` | Holds when                                              |
//! |------------|---------------------------------------------------------|
//! | `@filter`  | every list named in `filter` is non-empty after trimming |
//! | `@LIST`    | that list is non-empty after trimming                   |
//! | `!@LIST`   | the list was reported and *no* element passes           |
//!
//! A positive reference is replaced by the clauses it stands for before any
//! row is evaluated, so it merges with explicit `where` clauses on the same
//! list under the same-element rule. `!@LIST` is the one thing a plain clause
//! cannot say: `ACTIVE_ALARMS.level!:critical` means *some* alarm is not
//! critical, while `filter=ACTIVE_ALARMS.level:critical&where=!@ACTIVE_ALARMS`
//! means *no* alarm is.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use sismatic_api_types::{Alarm, FieldName, Read, ReadValue, RecordingEvent, ScheduleState};
use sismatic_store::group::satisfies;

use crate::handlers::error::ApiFailure;
use crate::handlers::fleet_reads::csv;
use crate::handlers::reads::normalize_field;

/// A parsed `where` and `filter` pair, ready to evaluate against rows.
#[derive(Debug, Default)]
pub(crate) struct Query {
    /// `where` clauses on a field's single value.
    scalar: Vec<Clause>,
    /// `where` attribute clauses, explicit or referenced, grouped by list.
    some: BTreeMap<FieldName, Vec<Clause>>,
    /// `!@LIST` references: lists in which no element may pass.
    none: BTreeMap<FieldName, Vec<Clause>>,
    /// `filter` clauses, grouped by list.
    filter: BTreeMap<FieldName, Vec<Clause>>,
}

impl Query {
    /// Parse the raw `?where=` and `?filter=` parameters.
    pub(crate) fn parse(
        where_raw: &Option<String>,
        filter_raw: &Option<String>,
    ) -> Result<Self, ApiFailure> {
        let mut query = Query::default();

        for raw in csv(filter_raw) {
            let clause = Clause::parse(raw)?;
            if !matches!(clause.target, Target::Attr(..)) {
                return Err(bad(format!(
                    "'{raw}' names no attribute; filter trims lists, so write a filter \
                     clause as 'LIST.attribute:value', e.g. \
                     'RECORDING_EVENTS.state:archive_failed'"
                )));
            }
            query
                .filter
                .entry(clause.field.clone())
                .or_default()
                .push(clause);
        }

        for raw in csv(where_raw) {
            match Term::parse(raw)? {
                Term::Clause(clause) => match clause.target {
                    Target::Attr(..) => {
                        query
                            .some
                            .entry(clause.field.clone())
                            .or_default()
                            .push(clause);
                    }
                    Target::Values(_) | Target::Ordered(_) => query.scalar.push(clause),
                },
                Term::Every => {
                    if query.filter.is_empty() {
                        return Err(bad(
                            "'@filter' refers to the filter clauses, and ?filter= has none"
                                .to_owned(),
                        ));
                    }
                    for (list, clauses) in &query.filter {
                        query
                            .some
                            .entry(list.clone())
                            .or_default()
                            .extend(clauses.iter().cloned());
                    }
                }
                Term::Some(list) => {
                    let clauses = query.referenced(raw, &list)?;
                    query.some.entry(list).or_default().extend(clauses);
                }
                Term::None(list) => {
                    let clauses = query.referenced(raw, &list)?;
                    query.none.insert(list, clauses);
                }
            }
        }

        Ok(query)
    }

    /// The filter clauses on `list`, which a reference in `raw` names.
    fn referenced(&self, raw: &str, list: &FieldName) -> Result<Vec<Clause>, ApiFailure> {
        self.filter.get(list).cloned().ok_or_else(|| {
            bad(format!(
                "'{raw}' refers to the filter clauses on {list}, and ?filter= has none; \
                 add one, e.g. '{list}.attribute:value'"
            ))
        })
    }

    /// Whether `latest`, one row's whole snapshot, satisfies `where`.
    pub(crate) fn selects(&self, latest: &[Read]) -> bool {
        self.checks().all(|check| check.holds(latest))
    }

    /// The conditions `where` imposes, one per field-level clause and one per
    /// list. A route whose row has several subjects (a group's members)
    /// quantifies over each check separately; see [`Check`].
    pub(crate) fn checks(&self) -> impl Iterator<Item = Check<'_>> {
        let scalar = self.scalar.iter().map(|clause| Check {
            field: &clause.field,
            kind: CheckKind::Scalar(clause),
        });
        let some = self.some.iter().map(|(field, clauses)| Check {
            field,
            kind: CheckKind::Some(clauses),
        });
        let none = self.none.iter().map(|(field, clauses)| Check {
            field,
            kind: CheckKind::None(clauses),
        });
        scalar.chain(some).chain(none)
    }

    /// Apply `filter`: keep only the list elements that pass. Never drops a read.
    pub(crate) fn narrow(&self, mut latest: Vec<Read>) -> Vec<Read> {
        for read in &mut latest {
            self.narrow_read(read);
        }
        latest
    }

    /// Apply `filter` to one read.
    pub(crate) fn narrow_read(&self, read: &mut Read) {
        let Some(clauses) = self.filter.get(&read.field) else {
            return;
        };
        match &mut read.value {
            ReadValue::RecordingEvents(events) => events.retain(|e| passes(clauses, e)),
            ReadValue::Alarms(alarms) => alarms.retain(|a| passes(clauses, a)),
            _ => {}
        }
    }

    /// Refuse `where` on a route that answers for one thing, so there are no
    /// rows to pick. `instead` names the index route that takes it.
    pub(crate) fn without_where(&self, instead: &str) -> Result<(), ApiFailure> {
        if self.checks().next().is_none() {
            return Ok(());
        }
        Err(bad(format!(
            "?where= picks rows, and this route answers for one thing, so there is \
             nothing for it to pick; ask {instead} instead, or use ?filter= to trim lists"
        )))
    }

    /// Refuse a clause about any field but `field`, on a route that reads
    /// only that one: it could never hold, or never trim anything.
    pub(crate) fn only_about(&self, field: &FieldName) -> Result<(), ApiFailure> {
        let named = self
            .checks()
            .map(|check| check.field)
            .chain(self.filter.keys());
        for other in named {
            if other != field {
                return Err(bad(format!(
                    "this route reads only {field}, so a clause on {other} could never \
                     apply; name {field} in the clause, or ask the route for {other}"
                )));
            }
        }
        Ok(())
    }
}

/// One condition of `where`, about one field.
///
/// Exposed so the group index can quantify per condition: a group satisfies
/// a check when every member that reported its field does. Quantifying the
/// whole `where` per member instead would drop a group whenever one member
/// had reported one field and not another.
pub(crate) struct Check<'a> {
    field: &'a FieldName,
    kind: CheckKind<'a>,
}

enum CheckKind<'a> {
    /// A field-level clause.
    Scalar(&'a Clause),
    /// Some element of the list passes every clause.
    Some(&'a [Clause]),
    /// The list was reported and no element passes.
    None(&'a [Clause]),
}

impl<'a> Check<'a> {
    /// The field this check reads.
    pub(crate) fn field(&self) -> &'a FieldName {
        self.field
    }

    /// Whether one subject's snapshot satisfies this check. A snapshot
    /// without a read of the field never does.
    pub(crate) fn holds(&self, latest: &[Read]) -> bool {
        let Some(read) = latest.iter().find(|read| &read.field == self.field) else {
            return false;
        };
        match self.kind {
            CheckKind::Scalar(clause) => clause.on_value(&read.value) == Some(true),
            CheckKind::Some(clauses) => elements(&read.value)
                .is_some_and(|items| items.iter().any(|item| passes(clauses, *item))),
            CheckKind::None(clauses) => elements(&read.value)
                .is_some_and(|items| !items.iter().any(|item| passes(clauses, *item))),
        }
    }
}

/// Whether one element passes every clause in `clauses`.
fn passes(clauses: &[Clause], item: &dyn Element) -> bool {
    clauses.iter().all(|clause| clause.on_element(item))
}

/// The elements of a list-valued read, or `None` for a single value.
fn elements(value: &ReadValue) -> Option<Vec<&dyn Element>> {
    match value {
        ReadValue::RecordingEvents(events) => Some(events.iter().map(|e| e as _).collect()),
        ReadValue::Alarms(alarms) => Some(alarms.iter().map(|a| a as _).collect()),
        _ => None,
    }
}

/// One entry of `?where=`.
enum Term {
    Clause(Clause),
    /// `@filter`
    Every,
    /// `@LIST`
    Some(FieldName),
    /// `!@LIST`
    None(FieldName),
}

impl Term {
    fn parse(raw: &str) -> Result<Self, ApiFailure> {
        let (name, negated) = if let Some(name) = raw.strip_prefix("!@") {
            (name, true)
        } else if let Some(name) = raw.strip_prefix('@') {
            (name, false)
        } else {
            return Clause::parse(raw).map(Term::Clause);
        };

        if name.eq_ignore_ascii_case("filter") {
            return if negated {
                Err(bad(format!(
                    "'{raw}' is not defined; name one list instead, e.g. '!@ACTIVE_ALARMS'"
                )))
            } else {
                Ok(Term::Every)
            };
        }
        let list = normalize_field(name.trim());
        if list.is_empty() {
            return Err(bad(format!(
                "'{raw}' names no list; write a reference as '@filter', '@LIST' or '!@LIST'"
            )));
        }
        Ok(if negated {
            Term::None(list)
        } else {
            Term::Some(list)
        })
    }
}

/// One parsed clause.
#[derive(Debug, Clone)]
struct Clause {
    field: FieldName,
    target: Target,
    op: Op,
    negated: bool,
}

/// What a clause compares, decided at parse time.
#[derive(Debug, Clone)]
enum Target {
    /// A field's single value, compared in the shape the device answered in.
    Values(Vec<String>),
    /// A field's single value, ordered against a number or dotted version.
    Ordered(Vec<u64>),
    /// One attribute of each list element, against values of its type.
    Attr(Attr, Vec<Want>),
}

impl Clause {
    fn parse(raw: &str) -> Result<Self, ApiFailure> {
        let (lhs, rest) = raw.split_once(':').ok_or_else(|| {
            bad(format!(
                "'{raw}' is not a clause; write one as 'FIELD:value' or \
                 'LIST.attribute:value', e.g. 'RUNNING_STATE:stopped' or \
                 'RECORDING_EVENTS.state:archive_failed'"
            ))
        })?;
        let (lhs, negated) = match lhs.strip_suffix('!') {
            Some(lhs) => (lhs, true),
            None => (lhs, false),
        };
        let (field, attr) = match lhs.split_once('.') {
            Some((field, attr)) => (field, Some(attr)),
            None => (lhs, None),
        };
        let field = normalize_field(field.trim());
        if field.is_empty() {
            return Err(bad(format!(
                "'{raw}' names no field; write a clause as 'FIELD:value'"
            )));
        }

        let (op, rest) = Op::split(rest);
        let values: Vec<&str> = rest.split('|').collect();
        if op.orders() && values.len() > 1 {
            return Err(bad(format!(
                "'{raw}' orders against several values; gt/ge/lt/le take exactly one"
            )));
        }

        let target = match attr {
            None if op.orders() => Target::Ordered(segments(values[0]).ok_or_else(|| {
                bad(format!(
                    "'{raw}': '{}' is not a number or a dotted version, which is what \
                     gt/ge/lt/le compare",
                    values[0]
                ))
            })?),
            None => Target::Values(values.iter().map(|v| (*v).to_owned()).collect()),
            Some(name) => {
                let (attr, kind) = Attr::parse(name).ok_or_else(|| {
                    bad(format!(
                        "'{raw}': '{name}' is not an attribute; the attributes are {}",
                        Attr::ALL.map(|(name, _, _)| name).join(", ")
                    ))
                })?;
                kind.admits(op)
                    .map_err(|reason| bad(format!("'{raw}': {name} {reason}")))?;
                let wants = values
                    .iter()
                    .map(|v| kind.want(v).ok_or_else(|| bad(kind.refusal(raw, v))))
                    .collect::<Result<_, _>>()?;
                Target::Attr(attr, wants)
            }
        };

        Ok(Clause {
            field,
            target,
            op,
            negated,
        })
    }

    /// The clause on a field's single value: `None` when the comparison does
    /// not apply to the read's shape, which fails the clause either way.
    fn on_value(&self, value: &ReadValue) -> Option<bool> {
        if elements(value).is_some() {
            return None;
        }
        let hit = match (&self.target, self.op) {
            (Target::Values(wants), Op::Eq) => wants
                .iter()
                .any(|want| satisfies(&ReadValue::Text(want.clone()), value)),
            (Target::Values(wants), Op::Has) => {
                let got = match value {
                    ReadValue::Text(got) | ReadValue::Version(got) | ReadValue::Ack(got) => got,
                    _ => return None,
                };
                wants.iter().any(|want| contains_folded(got, want))
            }
            (Target::Ordered(want), op) => {
                let got = match value {
                    ReadValue::Port(n) => vec![u64::from(*n)],
                    ReadValue::Number(n) => vec![u64::from(*n)],
                    ReadValue::Version(v) => segments(v)?,
                    _ => return None,
                };
                op.accepts(got.cmp(want))
            }
            _ => return None,
        };
        Some(hit != self.negated)
    }

    /// The clause on one list element. An element without the attribute fails.
    fn on_element(&self, item: &dyn Element) -> bool {
        let Target::Attr(attr, wants) = &self.target else {
            return false;
        };
        let Some(got) = item.attr(*attr) else {
            return false;
        };
        wants.iter().any(|want| self.op.compares(&got, want)) != self.negated
    }
}

/// A comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
    Has,
}

impl Op {
    /// Split a leading `gt:` (etc.) off a clause's right-hand side.
    fn split(rest: &str) -> (Op, &str) {
        for (prefix, op) in [
            ("gt:", Op::Gt),
            ("ge:", Op::Ge),
            ("lt:", Op::Lt),
            ("le:", Op::Le),
            ("has:", Op::Has),
        ] {
            if let Some(rest) = rest.strip_prefix(prefix) {
                return (op, rest);
            }
        }
        (Op::Eq, rest)
    }

    fn orders(self) -> bool {
        matches!(self, Op::Gt | Op::Ge | Op::Lt | Op::Le)
    }

    fn accepts(self, ord: Ordering) -> bool {
        match self {
            Op::Eq => ord.is_eq(),
            Op::Gt => ord.is_gt(),
            Op::Ge => ord.is_ge(),
            Op::Lt => ord.is_lt(),
            Op::Le => ord.is_le(),
            Op::Has => false,
        }
    }

    fn compares(self, got: &Leaf<'_>, want: &Want) -> bool {
        match (got, want) {
            (Leaf::Int(got), Want::Int(want)) => self.accepts(got.cmp(want)),
            (Leaf::Text(got), Want::Text(want)) => match self {
                Op::Has => contains_folded(got, want),
                _ => got == want,
            },
            (Leaf::Bool(got), Want::Bool(want)) => got == want,
            (Leaf::Schedule(got), Want::Schedule(want)) => got == want,
            _ => false,
        }
    }
}

/// An attribute of a list element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Attr {
    DbId,
    Start,
    Identifier,
    Creator,
    Title,
    State,
    MediaPresent,
    Package,
    Name,
    Level,
}

impl Attr {
    /// Every attribute, with its spelling and type. Recording events have the
    /// first eight, alarms the last two.
    const ALL: [(&'static str, Attr, Kind); 10] = [
        ("db_id", Attr::DbId, Kind::Int),
        ("start", Attr::Start, Kind::Int),
        ("identifier", Attr::Identifier, Kind::Text),
        ("creator", Attr::Creator, Kind::Text),
        ("title", Attr::Title, Kind::Text),
        ("state", Attr::State, Kind::Schedule),
        ("media_present", Attr::MediaPresent, Kind::Bool),
        ("package", Attr::Package, Kind::Text),
        ("name", Attr::Name, Kind::Text),
        ("level", Attr::Level, Kind::Text),
    ];

    fn parse(name: &str) -> Option<(Attr, Kind)> {
        let name = name.trim().replace('-', "_").to_ascii_lowercase();
        Self::ALL
            .iter()
            .find(|(spelling, _, _)| *spelling == name)
            .map(|(_, attr, kind)| (*attr, *kind))
    }
}

/// The type of an attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Int,
    Text,
    Bool,
    Schedule,
}

impl Kind {
    /// Whether `op` means anything for this type; the reason if not.
    fn admits(self, op: Op) -> Result<(), &'static str> {
        match (self, op) {
            (_, Op::Eq)
            | (Kind::Int, Op::Gt | Op::Ge | Op::Lt | Op::Le)
            | (Kind::Text, Op::Has) => Ok(()),
            (_, Op::Has) => Err("is not text, so 'has' cannot search it"),
            _ => Err("has no order, so gt/ge/lt/le cannot compare it"),
        }
    }

    fn want(self, text: &str) -> Option<Want> {
        match self {
            Kind::Int => text.trim().parse().ok().map(Want::Int),
            Kind::Text => Some(Want::Text(text.to_owned())),
            Kind::Bool => flag_of(text.trim()).map(Want::Bool),
            Kind::Schedule => ScheduleState::from_text(text).map(Want::Schedule),
        }
    }

    fn refusal(self, raw: &str, text: &str) -> String {
        let expected = match self {
            Kind::Int => "an integer",
            Kind::Text => "text",
            Kind::Bool => "a flag (true/false, on/off, yes/no, 1/0)",
            Kind::Schedule => {
                "a schedule state, e.g. 'archive_failed', 'SCHEDULE_ARCHIVE_FAILED' or '6'"
            }
        };
        format!("'{raw}': '{text}' is not {expected}")
    }
}

/// A clause's value, parsed into its attribute's type.
#[derive(Debug, Clone, PartialEq)]
enum Want {
    Int(i64),
    Text(String),
    Bool(bool),
    Schedule(ScheduleState),
}

/// One attribute's value on one element.
enum Leaf<'a> {
    Int(i64),
    Text(&'a str),
    Bool(bool),
    Schedule(ScheduleState),
}

/// A list element a clause can address by attribute.
trait Element {
    /// The attribute's value, or `None` if this element does not carry it.
    fn attr(&self, attr: Attr) -> Option<Leaf<'_>>;
}

impl Element for RecordingEvent {
    fn attr(&self, attr: Attr) -> Option<Leaf<'_>> {
        Some(match attr {
            Attr::DbId => Leaf::Int(i64::try_from(self.db_id).ok()?),
            Attr::Start => Leaf::Int(self.start?),
            Attr::Identifier => Leaf::Text(&self.identifier),
            Attr::Creator => Leaf::Text(&self.creator),
            Attr::Title => Leaf::Text(&self.title),
            Attr::State => Leaf::Schedule(self.state?),
            Attr::MediaPresent => Leaf::Bool(self.media_present?),
            Attr::Package => Leaf::Text(&self.package),
            Attr::Name | Attr::Level => return None,
        })
    }
}

impl Element for Alarm {
    fn attr(&self, attr: Attr) -> Option<Leaf<'_>> {
        match attr {
            Attr::Name => Some(Leaf::Text(&self.name)),
            Attr::Level => Some(Leaf::Text(&self.level)),
            _ => None,
        }
    }
}

/// A number or dotted version as numeric segments: `"2.11"` is `[2, 11]`.
fn segments(text: &str) -> Option<Vec<u64>> {
    text.trim()
        .split('.')
        .map(|part| part.parse().ok())
        .collect()
}

/// Case-insensitive substring test.
fn contains_folded(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// The flag spellings `sismatic-core`'s setting encoder accepts.
fn flag_of(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Some(true),
        "0" | "false" | "off" | "no" => Some(false),
        _ => None,
    }
}

fn bad(message: String) -> ApiFailure {
    ApiFailure::BadRequest(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sismatic_api_types::{RecordingState, Timestamp};

    fn read(field: &str, value: ReadValue) -> Read {
        Read {
            device: "atrium-101".to_owned(),
            field: field.to_owned(),
            value,
            at: Timestamp("2026-08-17T00:00:00.000Z".to_owned()),
        }
    }

    fn event(db_id: u64, state: Option<ScheduleState>, title: &str) -> RecordingEvent {
        RecordingEvent {
            db_id,
            start: Some(1_767_879_000 + db_id as i64),
            identifier: String::new(),
            creator: String::new(),
            title: title.to_owned(),
            state,
            media_present: Some(true),
            package: format!("/var/internal/P_{db_id}"),
            extra: Default::default(),
        }
    }

    fn alarm(name: &str, level: &str) -> Alarm {
        Alarm {
            name: name.to_owned(),
            level: level.to_owned(),
        }
    }

    fn events(rows: Vec<RecordingEvent>) -> Read {
        read("RECORDING_EVENTS", ReadValue::RecordingEvents(rows))
    }

    fn alarms(rows: Vec<Alarm>) -> Read {
        read("ACTIVE_ALARMS", ReadValue::Alarms(rows))
    }

    fn query(where_raw: &str, filter_raw: &str) -> Query {
        let some = |raw: &str| (!raw.is_empty()).then(|| raw.to_owned());
        Query::parse(&some(where_raw), &some(filter_raw)).expect("a well-formed query")
    }

    fn refused(where_raw: &str, filter_raw: &str) -> bool {
        let some = |raw: &str| (!raw.is_empty()).then(|| raw.to_owned());
        matches!(
            Query::parse(&some(where_raw), &some(filter_raw)),
            Err(ApiFailure::BadRequest(_))
        )
    }

    /// The three-device fleet the design discussion worked through.
    fn atrium() -> Vec<Read> {
        vec![
            events(vec![
                event(1, Some(ScheduleState::Archived), "Lecture 1"),
                event(2, Some(ScheduleState::ArchiveFailed), "Lecture 2"),
            ]),
            alarms(vec![alarm("video_loss", "critical")]),
        ]
    }
    fn annex() -> Vec<Read> {
        vec![
            events(vec![event(7, Some(ScheduleState::Archived), "Week 4")]),
            alarms(vec![]),
        ]
    }
    fn beacon() -> Vec<Read> {
        vec![
            events(vec![
                event(3, Some(ScheduleState::ArchiveFailed), "Week 4"),
                event(4, Some(ScheduleState::RecordFailed), "Week 5"),
            ]),
            alarms(vec![alarm("disk_low", "warning")]),
        ]
    }

    /// Which of the three devices `q` selects, by name.
    fn selected(q: &Query) -> Vec<&'static str> {
        [
            ("atrium", atrium()),
            ("annex", annex()),
            ("beacon", beacon()),
        ]
        .into_iter()
        .filter(|(_, latest)| q.selects(latest))
        .map(|(name, _)| name)
        .collect()
    }

    fn db_ids(latest: &[Read]) -> Vec<u64> {
        latest
            .iter()
            .flat_map(|read| match &read.value {
                ReadValue::RecordingEvents(events) => events.iter().map(|e| e.db_id).collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    fn alarm_names(latest: &[Read]) -> Vec<String> {
        latest
            .iter()
            .flat_map(|read| match &read.value {
                ReadValue::Alarms(alarms) => alarms.iter().map(|a| a.name.clone()).collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    // --- scalar clauses -------------------------------------------------

    #[test]
    fn a_scalar_clause_reads_its_value_in_the_shape_the_device_answered_in() {
        let latest = vec![
            read("RUNNING_STATE", ReadValue::State(RecordingState::Stopped)),
            read("HTTP_PORT", ReadValue::Port(8080)),
            read("DHCP_MODE", ReadValue::Flag(true)),
            read("FIRMWARE", ReadValue::Version("2.11".to_owned())),
            read("TITLE", ReadValue::Text("Mechanics I".to_owned())),
        ];
        for clause in [
            "RUNNING_STATE:stopped",
            "running-state:STOPPED",
            "RUNNING_STATE:started|stopped",
            "RUNNING_STATE!:started",
            "HTTP_PORT:8080",
            "HTTP_PORT:gt:8000",
            "HTTP_PORT:le:8080",
            "DHCP_MODE:on",
            "FIRMWARE:2.11",
            "FIRMWARE:gt:2.9",
            "FIRMWARE:lt:3",
            "TITLE:has:mechanics",
            "TITLE!:has:physics",
        ] {
            assert!(
                query(clause, "").selects(&latest),
                "{clause} should have matched"
            );
        }
        for clause in [
            "RUNNING_STATE:started",
            "RUNNING_STATE!:stopped",
            "HTTP_PORT:9090",
            "HTTP_PORT:gt:8080",
            "FIRMWARE:lt:2.9",
            "TITLE:has:physics",
        ] {
            assert!(
                !query(clause, "").selects(&latest),
                "{clause} should not have matched"
            );
        }
    }

    /// Negation does not reach a field that was never reported, nor a
    /// comparison that does not apply to the read's shape.
    #[test]
    fn a_clause_that_cannot_apply_fails_even_when_negated() {
        let latest = vec![read(
            "RUNNING_STATE",
            ReadValue::State(RecordingState::Stopped),
        )];
        for clause in [
            "FIRMWARE:2.11",
            "FIRMWARE!:2.11",
            "RUNNING_STATE:gt:1",
            "RUNNING_STATE!:gt:1",
            "RUNNING_STATE!:has:x",
        ] {
            assert!(!query(clause, "").selects(&latest), "{clause}");
        }
    }

    /// A list cannot be compared as one value; it needs an attribute.
    #[test]
    fn a_list_field_without_an_attribute_matches_nothing() {
        for clause in [
            "RECORDING_EVENTS:archive_failed",
            "RECORDING_EVENTS!:archive_failed",
        ] {
            assert_eq!(selected(&query(clause, "")), Vec::<&str>::new(), "{clause}");
        }
    }

    #[test]
    fn every_clause_has_to_hold() {
        let latest = vec![
            read("RUNNING_STATE", ReadValue::State(RecordingState::Stopped)),
            read("FIRMWARE", ReadValue::Version("2.11".to_owned())),
        ];
        assert!(query("RUNNING_STATE:stopped,FIRMWARE:2.11", "").selects(&latest));
        assert!(!query("RUNNING_STATE:stopped,FIRMWARE:2.09", "").selects(&latest));
    }

    // --- attribute clauses ----------------------------------------------

    #[test]
    fn an_attribute_clause_holds_when_some_element_passes() {
        assert_eq!(
            selected(&query("RECORDING_EVENTS.state:SCHEDULE_ARCHIVE_FAILED", "")),
            ["atrium", "beacon"]
        );
        assert_eq!(selected(&query("RECORDING_EVENTS.state:9", "")), ["beacon"]);
        assert_eq!(
            selected(&query("ACTIVE_ALARMS.level:critical", "")),
            ["atrium"]
        );
        assert_eq!(
            selected(&query("RECORDING_EVENTS.db_id:ge:4", "")),
            ["annex", "beacon"]
        );
        assert_eq!(
            selected(&query("RECORDING_EVENTS.title:has:week", "")),
            ["annex", "beacon"]
        );
    }

    /// `archive_failed` and `Week 4` exist together only on beacon's event 3.
    /// annex has `Week 4` without the failure, and atrium has the failure
    /// without `Week 4`.
    #[test]
    fn clauses_on_the_same_list_bind_to_the_same_element() {
        let q = query(
            "RECORDING_EVENTS.state:archive_failed,RECORDING_EVENTS.title:Week 4",
            "",
        );
        assert_eq!(selected(&q), ["beacon"]);

        let separate = vec![events(vec![
            event(1, Some(ScheduleState::ArchiveFailed), "Week 3"),
            event(2, Some(ScheduleState::Archived), "Week 4"),
        ])];
        assert!(!q.selects(&separate));
    }

    #[test]
    fn an_element_without_the_attribute_fails_even_when_negated() {
        let latest = vec![events(vec![event(1, None, "")])];
        assert!(!query("RECORDING_EVENTS.state:archived", "").selects(&latest));
        assert!(!query("RECORDING_EVENTS.state!:archived", "").selects(&latest));
        // An attribute of the other list kind is absent on every element.
        assert!(!query("ACTIVE_ALARMS.state:archived", "").selects(&atrium()));
    }

    // --- filter ---------------------------------------------------------

    #[test]
    fn filter_trims_lists_and_never_drops_a_read() {
        let q = query("", "RECORDING_EVENTS.state:archive_failed|record_failed");
        assert_eq!(db_ids(&q.narrow(atrium())), [2]);
        assert_eq!(db_ids(&q.narrow(beacon())), [3, 4]);

        let trimmed = q.narrow(annex());
        assert_eq!(trimmed.len(), annex().len());
        assert_eq!(db_ids(&trimmed), Vec::<u64>::new());
        // A list the filter does not name is untouched.
        assert_eq!(alarm_names(&q.narrow(atrium())), ["video_loss"]);
    }

    #[test]
    fn filter_clauses_on_one_list_must_all_pass() {
        let q = query(
            "",
            "RECORDING_EVENTS.state:archive_failed,RECORDING_EVENTS.title:Week 4",
        );
        assert_eq!(db_ids(&q.narrow(beacon())), [3]);
        assert_eq!(db_ids(&q.narrow(atrium())), Vec::<u64>::new());
    }

    #[test]
    fn filter_drops_elements_without_the_attribute() {
        let latest = vec![events(vec![
            event(1, None, ""),
            event(2, Some(ScheduleState::Archived), ""),
        ])];
        assert_eq!(
            db_ids(&query("", "RECORDING_EVENTS.state!:archive_failed").narrow(latest)),
            [2]
        );
    }

    // --- references -----------------------------------------------------

    /// Example 1 from the design: the admin's failed-upload worklist.
    #[test]
    fn every_reference_requires_each_filtered_list_to_be_non_empty() {
        let q = query("@filter", "RECORDING_EVENTS.state:archive_failed");
        assert_eq!(selected(&q), ["atrium", "beacon"]);
        assert_eq!(db_ids(&q.narrow(atrium())), [2]);
        assert_eq!(db_ids(&q.narrow(beacon())), [3]);
    }

    /// `@filter` is shorthand for the longhand: the same rows for every device.
    #[test]
    fn a_reference_selects_exactly_what_its_clauses_written_out_select() {
        let filter = "RECORDING_EVENTS.state:archive_failed,RECORDING_EVENTS.title:Week 4";
        assert_eq!(
            selected(&query("@filter", filter)),
            selected(&query(filter, ""))
        );
        assert_eq!(
            selected(&query("@RECORDING_EVENTS", filter)),
            selected(&query(filter, ""))
        );
    }

    /// Example 2: two lists trimmed, only one deciding the rows.
    #[test]
    fn a_list_reference_requires_only_that_list() {
        let filter = "RECORDING_EVENTS.state:archive_failed,ACTIVE_ALARMS.level:critical";
        let q = query("@RECORDING_EVENTS", filter);
        assert_eq!(selected(&q), ["atrium", "beacon"]);
        assert_eq!(alarm_names(&q.narrow(beacon())), Vec::<String>::new());

        assert_eq!(selected(&query("@filter", filter)), ["atrium"]);
    }

    /// Example 3: failed uploads and no critical alarm.
    #[test]
    fn a_negated_reference_requires_no_element_to_pass() {
        let filter = "RECORDING_EVENTS.state:archive_failed,ACTIVE_ALARMS.level:critical";
        let q = query("@RECORDING_EVENTS,!@ACTIVE_ALARMS", filter);
        assert_eq!(selected(&q), ["beacon"]);
    }

    /// Example 4: `!@` is "none", which `!:` ("some is not") cannot say.
    #[test]
    fn none_differs_from_some_element_not_matching() {
        assert_eq!(
            selected(&query("ACTIVE_ALARMS.level!:critical", "")),
            ["beacon"]
        );
        assert_eq!(
            selected(&query("!@ACTIVE_ALARMS", "ACTIVE_ALARMS.level:critical")),
            ["annex", "beacon"]
        );
    }

    /// "No element passes" is not claimed for a list the device never reported.
    #[test]
    fn a_negated_reference_needs_the_list_to_have_been_reported() {
        let q = query("!@ACTIVE_ALARMS", "ACTIVE_ALARMS.level:critical");
        assert!(!q.selects(&[read("FIRMWARE", ReadValue::Version("2.11".to_owned()))]));
    }

    /// Example 5: rows picked by less than the filter shows.
    #[test]
    fn where_and_filter_can_differ() {
        let q = query(
            "RECORDING_EVENTS.state:archive_failed",
            "RECORDING_EVENTS.state:archive_failed|record_failed",
        );
        assert_eq!(selected(&q), ["atrium", "beacon"]);
        assert_eq!(db_ids(&q.narrow(beacon())), [3, 4]);
    }

    /// A positive reference merges with explicit clauses on the same list.
    #[test]
    fn a_reference_and_an_explicit_clause_bind_to_the_same_element() {
        let q = query(
            "@filter,RECORDING_EVENTS.title:Week 4",
            "RECORDING_EVENTS.state:archive_failed",
        );
        assert_eq!(selected(&q), ["beacon"]);
    }

    #[test]
    fn references_combine_with_scalar_clauses() {
        let mut stopped = beacon();
        stopped.push(read(
            "RUNNING_STATE",
            ReadValue::State(RecordingState::Stopped),
        ));
        let q = query(
            "@filter,RUNNING_STATE:stopped",
            "RECORDING_EVENTS.state:archive_failed",
        );
        assert!(q.selects(&stopped));
        assert!(!q.selects(&atrium()));
    }

    // --- refusals -------------------------------------------------------

    #[test]
    fn malformed_clauses_are_refused_rather_than_ignored() {
        for (where_raw, filter_raw) in [
            ("RUNNING_STATE", ""),
            (":stopped", ""),
            (" :stopped", ""),
            ("RECORDING_EVENTS.status:archived", ""),
            ("RECORDING_EVENTS.state:archive-failed", ""),
            ("RECORDING_EVENTS.state:gt:5", ""),
            ("RECORDING_EVENTS.db_id:has:5", ""),
            ("RECORDING_EVENTS.db_id:five", ""),
            ("RECORDING_EVENTS.media_present:maybe", ""),
            ("RECORDING_EVENTS.start:gt:1|2", ""),
            ("HTTP_PORT:gt:high", ""),
            ("", "RUNNING_STATE:stopped"),
            ("", "RECORDING_EVENTS.state:archive-failed"),
            ("@filter", ""),
            ("@RECORDING_EVENTS", "ACTIVE_ALARMS.level:critical"),
            ("!@filter", "ACTIVE_ALARMS.level:critical"),
            ("@", "ACTIVE_ALARMS.level:critical"),
        ] {
            assert!(
                refused(where_raw, filter_raw),
                "where={where_raw} filter={filter_raw}"
            );
        }
    }

    #[test]
    fn field_attribute_and_reference_spellings_fold_case_and_dashes() {
        let q = query("@recording-events", "recording-events.STATE:archive_failed");
        assert_eq!(selected(&q), ["atrium", "beacon"]);
        assert_eq!(
            selected(&query("@FILTER", "RECORDING_EVENTS.state:6")),
            ["atrium", "beacon"]
        );
    }

    #[test]
    fn without_where_refuses_only_a_where() {
        assert!(
            query("", "RECORDING_EVENTS.state:archived")
                .without_where("/x")
                .is_ok()
        );
        assert!(matches!(
            query("RUNNING_STATE:stopped", "").without_where("/x"),
            Err(ApiFailure::BadRequest(_))
        ));
    }

    #[test]
    fn only_about_refuses_a_clause_on_another_field() {
        let events = "RECORDING_EVENTS".to_owned();
        assert!(
            query("@filter", "RECORDING_EVENTS.state:archived")
                .only_about(&events)
                .is_ok()
        );
        for (where_raw, filter_raw) in [
            ("RUNNING_STATE:stopped", ""),
            ("", "ACTIVE_ALARMS.level:critical"),
            ("!@ACTIVE_ALARMS", "ACTIVE_ALARMS.level:critical"),
        ] {
            assert!(
                matches!(
                    query(where_raw, filter_raw).only_about(&events),
                    Err(ApiFailure::BadRequest(_))
                ),
                "where={where_raw} filter={filter_raw}"
            );
        }
    }

    /// Each check reads one field, which is what a per-member quantifier needs.
    #[test]
    fn checks_name_their_fields() {
        let q = query(
            "RUNNING_STATE:stopped,@RECORDING_EVENTS,!@ACTIVE_ALARMS",
            "RECORDING_EVENTS.state:archived,ACTIVE_ALARMS.level:critical",
        );
        let fields: Vec<&str> = q.checks().map(|c| c.field().as_str()).collect();
        assert_eq!(
            fields,
            ["RUNNING_STATE", "RECORDING_EVENTS", "ACTIVE_ALARMS"]
        );
    }

    #[test]
    fn an_empty_query_selects_everything_and_trims_nothing() {
        let q = query("", "");
        assert_eq!(selected(&q), ["atrium", "annex", "beacon"]);
        assert_eq!(db_ids(&q.narrow(beacon())), [3, 4]);
    }
}
