use std::collections::VecDeque;
use std::sync::Arc;

use mlua::{Lua, LuaSerdeExt, Result as LuaResult, SerializeOptions, Value};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;

type Queue = Arc<Mutex<VecDeque<Box<dyn Request + Send + Sync>>>>;
type Subscriptions<E> = Arc<Mutex<Vec<mpsc::Sender<E>>>>;

pub struct PendingRequest<P, R>
where
    P: Serialize,
    for<'de> R: Deserialize<'de>,
{
    method: String,
    params: Option<P>,
    tx: Option<oneshot::Sender<Response<R>>>,
}

pub trait Request {
    fn method(&self) -> &str;
    fn params<'lua>(&self, lua: &'lua Lua) -> Result<Option<Value<'lua>>, mlua::Error>;
    fn success<'lua>(&mut self, lua: &'lua Lua, value: &Value) -> Result<(), mlua::Error>;
    fn error(&mut self, error: String, kind: Option<String>);
}

pub struct IPC<E> {
    queue: Queue,
    subscriptions: Subscriptions<E>,
}

impl<E> IPC<E> {
    pub fn try_next(&self) -> Option<Box<dyn Request + Send + Sync>> {
        if let Ok(mut queue) = self.queue.try_lock() {
            queue.pop_front()
        } else {
            None
        }
    }

    pub async fn event(&self, event: E)
    where
        E: Clone + std::fmt::Debug,
    {
        let mut clients = self.subscriptions.lock().await;
        clients.retain_mut(move |tx| match tx.try_send(event.clone()) {
            Ok(_) => true,
            Err(TrySendError::Full(_)) => {
                log::error!(
                    "IPC event channel is full and cannot receive any more events right now"
                );
                true
            }
            Err(TrySendError::Closed(_)) => false,
        });
    }

    pub async fn request<P, R>(&self, method: &str, params: Option<P>) -> Result<R, Error>
    where
        P: serde::Serialize + Send + Sync + 'static,
        for<'de> R: serde::Deserialize<'de> + Send + Sync + std::fmt::Debug + 'static,
    {
        let (tx, rx) = oneshot::channel();
        {
            let mut queue = self.queue.lock().await;
            queue.push_back(Box::new(PendingRequest {
                method: method.to_string(),
                params,
                tx: Some(tx),
            }));
        }

        // TODO: add timeout
        let res = rx.await.unwrap();
        match res {
            Response::Success(result) => Ok(result),
            Response::Error { kind, message } => Err(Error::Script { kind, message }),
        }
    }

    pub async fn notification<P>(&self, method: &str, params: Option<P>) -> Result<(), Error>
    where
        P: serde::Serialize + Send + Sync + 'static,
    {
        let (tx, rx) = oneshot::channel::<Response<()>>();
        {
            let mut queue = self.queue.lock().await;
            queue.push_back(Box::new(PendingRequest {
                method: method.to_string(),
                params,
                tx: Some(tx),
            }));
        }

        rx.await.unwrap();
        Ok(())
    }

    pub async fn events(&self) -> impl Stream<Item = E> {
        let (tx, rx) = mpsc::channel(1024);
        {
            let mut subscriptions = self.subscriptions.lock().await;
            subscriptions.push(tx);
        }
        ReceiverStream::new(rx)
    }
}

impl<E> Default for IPC<E> {
    fn default() -> Self {
        Self {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Error from mission script: {message}")]
    Script {
        kind: Option<String>,
        message: String,
    },
    #[error("Failed to deserialize params: {0}")]
    DeserializeParams(#[source] mlua::Error),
    #[error("Failed to deserialize result for method {method}: {err}\n{result}")]
    DeserializeResult {
        #[source]
        err: mlua::Error,
        method: String,
        result: String,
    },
    #[error("Failed to serialize params: {0}")]
    SerializeParams(#[source] mlua::Error),
}

impl<E> Clone for IPC<E> {
    fn clone(&self) -> Self {
        IPC {
            queue: self.queue.clone(),
            subscriptions: self.subscriptions.clone(),
        }
    }
}

impl<P, R> Request for PendingRequest<P, R>
where
    P: Serialize,
    for<'de> R: Deserialize<'de> + std::fmt::Debug,
{
    fn method(&self) -> &str {
        &self.method
    }

    fn params<'lua>(&self, lua: &'lua Lua) -> Result<Option<Value<'lua>>, mlua::Error> {
        self.params
            .as_ref()
            .map(|params| {
                lua.to_value_with(
                    params,
                    SerializeOptions::new().serialize_none_to_null(false),
                )
            })
            .transpose()
    }

    fn success<'lua>(&mut self, lua: &'lua Lua, value: &Value) -> Result<(), mlua::Error> {
        let res = lua.from_value(value.clone())?;
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Response::Success(res));
        } else {
            log::error!("Failed to send IPC success result: channel gone");
        }

        Ok(())
    }

    fn error(&mut self, message: String, kind: Option<String>) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Response::Error { kind, message });
        } else {
            log::error!("Failed to send IPC error result: channel gone");
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response<R> {
    Success(R),
    Error {
        kind: Option<String>,
        message: String,
    },
}

#[allow(unused)]
fn pretty_print_value(val: Value<'_>, indent: usize) -> LuaResult<String> {
    Ok(match val {
        Value::Nil => "nil".to_string(),
        Value::Boolean(v) => v.to_string(),
        Value::LightUserData(_) => String::new(),
        Value::Integer(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::String(v) => format!("\"{}\"", v.to_str()?),
        Value::Table(t) => {
            let mut s = "{\n".to_string();
            for pair in t.pairs::<Value<'_>, Value<'_>>() {
                let (key, value) = pair?;
                s += &format!(
                    "{}{} = {},\n",
                    "  ".repeat(indent + 1),
                    pretty_print_value(key, indent + 1)?,
                    pretty_print_value(value, indent + 1)?
                );
            }
            s += &format!("{}}}", "  ".repeat(indent));
            s
        }
        Value::Function(_) => "[function]".to_string(),
        Value::Thread(_) => String::new(),
        Value::UserData(_) => String::new(),
        Value::Error(err) => err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::Error;

    #[test]
    fn test_error_send_sync() {
        fn assert_send_sync(_: impl std::error::Error + Send + Sync) {}
        assert_send_sync(Error::Script {
            kind: None,
            message: String::new(),
        });
    }
}
