use std::string::ToString;

use bugzilla_query::Bug;
use color_eyre::eyre::{bail, eyre, Context, Result};
use jira_query::Issue;

// use crate::config::tracker::Service;
use crate::config::{tracker, TicketQuery};
use crate::ticket_abstraction::{AbstractTicket, IntoAbstract};

// The number of items in a single Jira query.
// All Jira queries are processed in chunks of this size.
// This prevents hitting the maximum allowed request size set in the Jira instance.
// TODO: Make this configurable.
const JIRA_CHUNK_SIZE: u32 = 30;

// Always include these fields in Bugzilla requests. We process some of their content.
const BZ_INCLUDED_FIELDS: &[&str; 3] = &["_default", "pool", "flags"];

pub struct AnnotatedTicket<'a> {
    pub ticket: AbstractTicket,
    pub query: &'a TicketQuery,
}

/// Prepare a client to access Bugzilla.
fn bz_instance(trackers: &tracker::Config) -> Result<bugzilla_query::BzInstance> {
    let api_key = if let Some(key) = &trackers.bugzilla.api_key {
        key.clone()
    } else {
        // TODO: Store the name of the variable in a constant, or make it configurable.
        std::env::var("BZ_API_KEY").context("Set the BZ_API_KEY environment variable.")?
    };

    Ok(
        bugzilla_query::BzInstance::at(trackers.bugzilla.host.clone())?
            .authenticate(bugzilla_query::Auth::ApiKey(api_key))?
            .paginate(bugzilla_query::Pagination::Unlimited)
            .include_fields(BZ_INCLUDED_FIELDS.iter().map(ToString::to_string).collect()),
    )
}
/// Prepare a client to access Jira.
fn jira_instance(trackers: &tracker::Config) -> Result<jira_query::JiraInstance> {
    let api_key = if let Some(key) = &trackers.jira.api_key {
        key.clone()
    } else {
        // TODO: Store the name of the variable in a constant, or make it configurable.
        std::env::var("JIRA_API_KEY").context("Set the JIRA_API_KEY environment variable.")?
    };

    Ok(jira_query::JiraInstance::at(trackers.jira.host.clone())?
        .authenticate(jira_query::Auth::ApiKey(api_key))?
        .paginate(jira_query::Pagination::ChunkSize(JIRA_CHUNK_SIZE)))
}

// TODO: Consider adding progress bars here. Investigate these libraries:
// * https://crates.io/crates/progressing
// * https://crates.io/crates/linya
// * https://crates.io/crates/indicatif
/// Process the configured ticket queries into abstract tickets,
/// sorted in no particular order, which depends on the response from the issue tracker.
///
/// Downloads from Bugzilla and from Jira in parallel.
#[tokio::main]
pub async fn unsorted_tickets<'a>(
    queries: &'a [TicketQuery],
    trackers: &tracker::Config,
) -> Result<Vec<AnnotatedTicket<'a>>> {
    // If no queries were found in the project configuration, quit with an error.
    // Such a situation should never occur because our config parsing requires at least
    // some items in the tickets file, but better make sure.
    if queries.is_empty() {
        bail!("No tickets are configured in this project.");
    }

    // Download from Bugzilla and from Jira in parallel:
    let bugs = bugs(queries, trackers);
    let issues = issues(queries, trackers);

    // Wait until both downloads have finished:
    let (bugs, issues) = tokio::join!(bugs, issues);

    let mut results = Vec::new();

    // Convert bugs and issues into abstract tickets.
    // Using an imperative style so that each `into_abstract` call can return an error.
    for (query, bug) in bugs? {
        let ticket = bug.into_abstract(&trackers.bugzilla)?;
        let annotated = AnnotatedTicket { ticket, query };
        results.push(annotated);
    }
    for (query, issue) in issues? {
        let ticket = issue.into_abstract(&trackers.jira)?;
        let annotated = AnnotatedTicket { ticket, query };
        results.push(annotated);
    }

    Ok(results)
}

/// Download all configured bugs from Bugzilla.
/// Returns every bug in a tuple, annotated with the query that it came from.
async fn bugs<'a>(
    queries: &'a [TicketQuery],
    trackers: &tracker::Config,
) -> Result<Vec<(&'a TicketQuery, Bug)>> {
    let bugzilla_queries_by_id: Vec<&str> = queries
        .iter()
        .filter(|&tq| tq.tracker() == &tracker::Service::Bugzilla)
        .filter_map(|tq| tq.key())
        .collect();

    let bugzilla_queries_by_search: Vec<&TicketQuery> = queries
        .iter()
        .filter(|&tq| tq.tracker() == &tracker::Service::Bugzilla)
        .filter(|&tq| tq.search().is_some())
        .collect();

    // If no tickets target Bugzilla, skip the download and return an empty vector.
    if bugzilla_queries_by_id.is_empty() && bugzilla_queries_by_search.is_empty() {
        return Ok(Vec::new());
    }

    log::info!("Downloading bugs from Bugzilla.");
    let bz_instance = bz_instance(trackers)?;

    let mut all_bugs = Vec::new();

    if !bugzilla_queries_by_id.is_empty() {
        let bugs = bz_instance
            .bugs(&bugzilla_queries_by_id)
            // This enables the download concurrency:
            .await
            .context("Failed to download tickets from Bugzilla.")?;

        let mut annotated_bugs: Vec<(&TicketQuery, Bug)> = Vec::new();

        for bug in bugs {
            let matching_query = queries
                .iter()
                .find(|query| query.key() == Some(bug.id.to_string().as_str()))
                .expect("Bug doesn't match any configured query.");
            annotated_bugs.push((matching_query, bug));
        }

        all_bugs.append(&mut annotated_bugs);
    }

    if !bugzilla_queries_by_search.is_empty() {
        let mut annotated_bugs: Vec<(&TicketQuery, Bug)> = Vec::new();

        for query in &bugzilla_queries_by_search {
            let bugs = bz_instance
                .query(query.search().unwrap())
                // This enables the download concurrency:
                .await
                .context("Failed to download tickets from Bugzilla.")?;

            for bug in bugs {
                annotated_bugs.push((query, bug));
            }
        }

        all_bugs.append(&mut annotated_bugs);
    }

    log::info!("Finished downloading from Bugzilla.");

    Ok(all_bugs)
}

/// Download all configured issues from Jira.
/// Returns every issue in a tuple, annotated with the query that it came from.
async fn issues<'a>(
    queries: &'a [TicketQuery],
    trackers: &tracker::Config,
) -> Result<Vec<(&'a TicketQuery, Issue)>> {
    let jira_queries: Vec<&TicketQuery> = queries
        .iter()
        .filter(|&t| t.tracker() == &tracker::Service::Jira)
        .collect();

    // If no tickets target Jira, skip the download and return an empty vector.
    if jira_queries.is_empty() {
        Ok(Vec::new())
    } else {
        let jira_instance = jira_instance(trackers)?;

        log::info!("Downloading issues from Jira.");

        let issues = jira_instance
            .issues(
                &jira_queries
                    .iter()
                    .filter_map(|q| q.key())
                    .collect::<Vec<&str>>(),
            )
            // This enables the download concurrency:
            .await
            .context("Failed to download tickets from Jira.")?;

        let mut annotated_issues: Vec<(&TicketQuery, Issue)> = Vec::new();

        for issue in issues {
            let matching_query = queries
                .iter()
                .find(|query| query.key() == Some(issue.key.as_str()))
                .ok_or_else(|| eyre!("Issue {} doesn't match any configured query.", issue.id))?;
            annotated_issues.push((matching_query, issue));
        }

        log::info!("Finished downloading from Jira.");

        Ok(annotated_issues)
    }
}

// Temporarily disable this function while converting to configurable fields.
/*
/// Process a single ticket specified using the `ticket` subcommand.
#[tokio::main]
pub async fn ticket<'a>(
    id: &str,
    api_key: &str,
    service: Service,
    tracker: &'a tracker::Instance,
) -> Result<AbstractTicket<'a>> {
    match service {
        tracker::Service::Jira => {
            let jira_instance = jira_query::JiraInstance::at(host.to_string())?
                .authenticate(jira_query::Auth::ApiKey(api_key.to_string()))?;

            let issue = jira_instance.issue(id).await?;
            Ok(issue.into_abstract())
        }
        tracker::Service::Bugzilla => {
            let bz_instance = bugzilla_query::BzInstance::at(host.to_string())?
                .authenticate(bugzilla_query::Auth::ApiKey(api_key.to_string()))?
                .include_fields(BZ_INCLUDED_FIELDS.iter().map(ToString::to_string).collect());

            let bug = bz_instance.bug(id).await?;
            Ok(bug.into_abstract())
        }
    }
}
*/
