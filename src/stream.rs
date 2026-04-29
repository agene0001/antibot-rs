//! Batch-oriented [`solve_stream`] returning a [`Stream`] of solved pages.
//!
//! Caller controls concurrency. Order is *not* preserved — completed solves
//! yield as they finish, which is what scrapers normally want.

use crate::client::Antibot;
use crate::error::AntibotError;
use crate::request::SolveRequest;
use crate::types::Solution;
use futures::stream::{Stream, StreamExt};
use std::pin::Pin;

pub type SolveStream<'a> =
    Pin<Box<dyn Stream<Item = (String, Result<Solution, AntibotError>)> + Send + 'a>>;

impl Antibot {
    /// Solve many URLs with bounded concurrency. The stream yields `(url, result)`
    /// pairs as solves complete.
    pub fn solve_stream<I>(&self, urls: I, concurrency: usize) -> SolveStream<'_>
    where
        I: IntoIterator<Item = String> + Send + 'static,
    {
        let client = self.clone();
        let concurrency = concurrency.max(1);

        let urls_vec: Vec<String> = urls.into_iter().collect();
        let stream = futures::stream::iter(urls_vec)
            .map(move |url| {
                let c = client.clone();
                async move {
                    let res = c.solve(&url).await;
                    (url, res)
                }
            })
            .buffer_unordered(concurrency);

        Box::pin(stream)
    }

    /// Same as [`Antibot::solve_stream`] but takes full [`SolveRequest`]s.
    pub fn execute_stream<I>(&self, requests: I, concurrency: usize) -> SolveStream<'_>
    where
        I: IntoIterator<Item = SolveRequest> + Send + 'static,
    {
        let client = self.clone();
        let concurrency = concurrency.max(1);

        let requests_vec: Vec<SolveRequest> = requests.into_iter().collect();
        let stream = futures::stream::iter(requests_vec)
            .map(move |req| {
                let c = client.clone();
                async move {
                    let url = req.url.clone();
                    let res = c.execute(req).await;
                    (url, res)
                }
            })
            .buffer_unordered(concurrency);

        Box::pin(stream)
    }
}

#[allow(unused_imports)]
pub use futures::StreamExt as _;
