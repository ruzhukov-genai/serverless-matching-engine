//! Dragonfly Streams helpers — XADD, XREADGROUP, XACK, XGROUP CREATE wrappers.
//!
//! Data is serialized/deserialized using MessagePack (rmp-serde).

use anyhow::{bail, Context, Result};
use deadpool_redis::Pool;
use serde::{de::DeserializeOwned, Serialize};

/// A single message from a stream.
pub struct StreamMessage {
    pub id: String,
    pub data: Vec<u8>,
}

/// Create a consumer group on a stream, creating the stream if it doesn't exist.
/// Idempotent — ignores "already exists" errors.
pub async fn create_consumer_group(pool: &Pool, stream: &str, group: &str) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let result: redis::RedisResult<()> = redis::cmd("XGROUP")
        .arg("CREATE")
        .arg(stream)
        .arg(group)
        .arg("$")
        .arg("MKSTREAM")
        .query_async(&mut *conn)
        .await;

    match result {
        Ok(()) => Ok(()),
        Err(e) if e.to_string().contains("BUSYGROUP") => Ok(()), // already exists — ok
        Err(e) => Err(e).context("XGROUP CREATE"),
    }
}

/// Publish raw bytes to a stream (XADD). Returns the message ID.
pub async fn publish_bytes(pool: &Pool, stream: &str, data: &[u8]) -> Result<String> {
    let mut conn = pool.get().await.context("pool.get")?;
    let id: String = redis::cmd("XADD")
        .arg(stream)
        .arg("*")
        .arg("d")
        .arg(data)
        .query_async(&mut *conn)
        .await
        .context("XADD")?;
    Ok(id)
}

/// Serialize a value with MessagePack and publish to a stream.
pub async fn publish<T: Serialize>(pool: &Pool, stream: &str, value: &T) -> Result<String> {
    let bytes = rmp_serde::to_vec_named(value).context("msgpack encode")?;
    publish_bytes(pool, stream, &bytes).await
}

/// Consume up to `count` messages from a consumer group (XREADGROUP).
/// Blocks for up to 1 second if no messages are available.
pub async fn consume(
    pool: &Pool,
    stream: &str,
    group: &str,
    consumer: &str,
    count: usize,
) -> Result<Vec<StreamMessage>> {
    let mut conn = pool.get().await.context("pool.get")?;

    let reply: redis::Value = redis::cmd("XREADGROUP")
        .arg("GROUP")
        .arg(group)
        .arg(consumer)
        .arg("COUNT")
        .arg(count)
        .arg("BLOCK")
        .arg(1000u64)
        .arg("STREAMS")
        .arg(stream)
        .arg(">")
        .query_async(&mut *conn)
        .await
        .context("XREADGROUP")?;

    parse_stream_reply(reply)
}

/// Deserialize a StreamMessage's data using MessagePack.
pub fn deserialize<T: DeserializeOwned>(msg: &StreamMessage) -> Result<T> {
    rmp_serde::from_slice(&msg.data).context("msgpack decode")
}

/// Acknowledge a message in a consumer group.
pub async fn ack(pool: &Pool, stream: &str, group: &str, id: &str) -> Result<()> {
    let mut conn = pool.get().await.context("pool.get")?;
    let _: i64 = redis::cmd("XACK")
        .arg(stream)
        .arg(group)
        .arg(id)
        .query_async(&mut *conn)
        .await
        .context("XACK")?;
    Ok(())
}

// ── Reply parser ──────────────────────────────────────────────────────────────

fn parse_stream_reply(reply: redis::Value) -> Result<Vec<StreamMessage>> {
    use redis::Value;

    // XREADGROUP returns: [[stream_name, [[id, [field, value, ...]], ...]]]
    // or Nil if no messages
    match reply {
        Value::Nil => Ok(vec![]),
        Value::Array(streams) => {
            let mut out = Vec::new();
            for stream_entry in streams {
                if let Value::Array(parts) = stream_entry {
                    // parts[0] = stream name, parts[1] = array of messages
                    if parts.len() < 2 {
                        continue;
                    }
                    if let Value::Array(messages) = &parts[1] {
                        for msg in messages {
                            if let Value::Array(msg_parts) = msg {
                                if msg_parts.len() < 2 {
                                    continue;
                                }
                                let id = extract_string(&msg_parts[0])?;
                                let data = extract_field_data(&msg_parts[1])?;
                                out.push(StreamMessage { id, data });
                            }
                        }
                    }
                }
            }
            Ok(out)
        }
        _ => bail!("unexpected XREADGROUP reply type"),
    }
}

fn extract_string(v: &redis::Value) -> Result<String> {
    use redis::Value;
    match v {
        Value::BulkString(b) => Ok(String::from_utf8_lossy(b).into_owned()),
        Value::SimpleString(s) => Ok(s.clone()),
        other => bail!("expected string, got {:?}", other),
    }
}

fn extract_field_data(v: &redis::Value) -> Result<Vec<u8>> {
    use redis::Value;
    // Field-value pairs: ["d", <bytes>]
    if let Value::Array(pairs) = v {
        let mut i = 0;
        while i + 1 < pairs.len() {
            let field = extract_string(&pairs[i]).unwrap_or_default();
            if field == "d" {
                return match &pairs[i + 1] {
                    Value::BulkString(b) => Ok(b.clone()),
                    other => bail!("expected bulk string for field 'd', got {:?}", other),
                };
            }
            i += 2;
        }
        bail!("field 'd' not found in stream message")
    } else {
        bail!("expected array of field-value pairs")
    }
}
