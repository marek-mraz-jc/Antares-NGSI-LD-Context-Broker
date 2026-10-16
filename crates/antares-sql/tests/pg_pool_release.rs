// SPDX-License-Identifier: EUPL-1.2
//! T-4298: a connection the pool hands back is tested within a bound, so a
//! stalled stream costs the pool nothing. sqlx tests a released connection
//! with a ping it never times out; through a mesh proxy, which accepts the
//! bytes, a stream whose far side stalled answers that ping never, and each
//! one held its pool slot for hours (dev 2026-10-16: 20 of 20 slots taken,
//! the database showing 20 idle sessions, the oldest 2 h 09 min old).
//! Skips loudly without ANTARES_TEST_DATABASE_URL (see tests/pg.rs recipe).

#![cfg(feature = "postgres")]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

macro_rules! require_db {
    () => {
        match std::env::var("ANTARES_TEST_DATABASE_URL") {
            Ok(url) => url,
            Err(_) => {
                eprintln!("SKIP: ANTARES_TEST_DATABASE_URL not set");
                return;
            }
        }
    };
}

/// A TCP relay to the database whose open streams can be frozen: it keeps
/// both sockets open and stops forwarding, as a stalled mesh stream does.
/// Streams opened after the freeze relay normally.
struct Relay {
    url: String,
    open: Arc<Mutex<Vec<Arc<AtomicBool>>>>,
}

impl Relay {
    async fn start(url: &str) -> Self {
        // postgres://user[:password]@host:port/db: the authority sits between the
        // last `@` before the path and the path's `/`.
        let rest = url.split_once("://").expect("scheme").1;
        let (authority, path) = rest.split_once('/').expect("path");
        let (user, upstream) = authority.rsplit_once('@').expect("user@host");
        let upstream = if upstream.contains(':') {
            upstream.to_owned()
        } else {
            format!("{upstream}:5432")
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let through = format!("postgres://{user}@127.0.0.1:{port}/{path}");
        let open: Arc<Mutex<Vec<Arc<AtomicBool>>>> = Arc::default();
        let streams = open.clone();
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let Ok(server) = TcpStream::connect(&upstream).await else {
                    continue;
                };
                let frozen = Arc::new(AtomicBool::new(false));
                streams.lock().expect("lock").push(frozen.clone());
                let (cr, cw) = client.into_split();
                let (sr, sw) = server.into_split();
                tokio::spawn(pipe(cr, sw, frozen.clone()));
                tokio::spawn(pipe(sr, cw, frozen));
            }
        });
        Self { url: through, open }
    }

    fn freeze(&self) {
        for frozen in self.open.lock().expect("lock").iter() {
            frozen.store(true, Ordering::SeqCst);
        }
    }
}

async fn pipe(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    frozen: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let Ok(n) = from.read(&mut buf).await else {
            return;
        };
        if n == 0 {
            return;
        }
        if frozen.load(Ordering::SeqCst) {
            // Holds both halves open and forwards nothing, ever.
            std::future::pending::<()>().await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

#[tokio::test]
async fn cancelled_queries_give_their_connection_back() {
    let url = require_db!();
    let pool = antares_sql::store::pg::connect_with(&url, 2, Duration::from_secs(30))
        .await
        .expect("pool");
    for round in 0..40 {
        let p = pool.clone();
        let cancelled = tokio::time::timeout(Duration::from_millis(30), async move {
            if round % 2 == 0 {
                sqlx::query("SELECT pg_sleep(0.5)")
                    .execute(&p)
                    .await
                    .map(|_| ())
            } else {
                let mut tx = p.begin().await?;
                sqlx::query("SELECT pg_sleep(0.5)")
                    .execute(&mut *tx)
                    .await?;
                tx.commit().await
            }
        })
        .await;
        assert!(cancelled.is_err(), "round {round} finished inside 30 ms");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let a = pool
        .begin()
        .await
        .expect("first connection after the cancellations");
    let b = pool
        .begin()
        .await
        .expect("second connection after the cancellations");
    drop((a, b));
}

#[tokio::test]
async fn a_stalled_stream_does_not_keep_its_pool_slot() {
    let url = require_db!();
    let relay = Relay::start(&url).await;
    let pool = antares_sql::store::pg::connect_with(&relay.url, 2, Duration::from_secs(30))
        .await
        .expect("pool through the relay");
    // Two requests are reading when their streams stall; their callers give
    // up, as an edge does at its timeout, and drop them mid-read.
    let reads: Vec<_> = (0..2)
        .map(|_| {
            let p = pool.clone();
            tokio::spawn(tokio::time::timeout(
                Duration::from_millis(1500),
                async move { sqlx::query("SELECT pg_sleep(1)").execute(&p).await },
            ))
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(300)).await;
    relay.freeze();
    for read in reads {
        assert!(
            read.await.expect("joined").is_err(),
            "a stalled stream answered"
        );
    }
    // Within a few seconds both slots serve again, over fresh streams.
    let started = std::time::Instant::now();
    let p = pool.clone();
    let served = tokio::time::timeout(Duration::from_secs(15), async move {
        let mut a = p.begin().await?;
        let mut b = p.begin().await?;
        sqlx::query("SELECT 1").execute(&mut *a).await?;
        sqlx::query("SELECT 1").execute(&mut *b).await?;
        Ok::<_, sqlx::Error>(())
    })
    .await;
    assert!(
        matches!(served, Ok(Ok(()))),
        "the pool did not recover from two stalled streams: {served:?} after {:?} (size {}, idle {})",
        started.elapsed(),
        pool.size(),
        pool.num_idle()
    );
}
