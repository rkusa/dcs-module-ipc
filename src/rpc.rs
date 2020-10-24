use std::collections::VecDeque;
use std::sync::Arc;

use crate::retain_mut::RetainMut;
use futures::channel::{mpsc, oneshot};
use futures::lock::Mutex;
use futures::Stream;
use mlua::{Lua, Result as LuaResult, Value};
use serde::{Deserialize, Serialize};
use thiserror::Error;

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
    fn params<'lua>(&self, lua: &'lua Lua) -> Result<Option<Value<'lua>>, serde_mlua::Error>;
    fn success(&mut self, value: &Value) -> Result<(), serde_mlua::Error>;
    fn error(&mut self, error: String);
}

pub struct RPC<E> {
    queue: Queue,
    subscriptions: Subscriptions<E>,
}

impl<E> RPC<E> {
    pub fn new() -> Self {
        RPC {
            queue: Arc::new(Mutex::new(VecDeque::new())),
            subscriptions: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn try_next(&self) -> Option<Box<dyn Request + Send + Sync>> {
        if let Some(mut queue) = self.queue.try_lock() {
            queue.pop_front()
        } else {
            None
        }
    }

    pub async fn event(&self, event: E)
    where
        E: Clone + std::fmt::Debug,
    {
        log::debug!("Received event: {:#?}", event);
        let mut clients = self.subscriptions.lock().await;
        clients.retain_mut(move |tx| tx.try_send(event.clone()).is_ok());
    }

    pub async fn request<P, R>(&self, method: &str, params: Option<P>) -> Result<R, Error>
    where
        P: serde::Serialize + Send + Sync + 'static,
        for<'de> R: serde::Deserialize<'de> + Send + Sync + std::fmt::Debug + 'static,
    {
        let (tx, rx) = oneshot::channel();
        {
            // TODO: use async Mutex
            let mut queue = self.queue.lock().await;
            queue.push_back(Box::new(PendingRequest {
                method: method.to_string(),
                params: params,
                tx: Some(tx),
            }));
        }

        let res = rx.await.unwrap();
        match res {
            Response::Success(result) => Ok(result),
            Response::Error(msg) => Err(Error::Script(msg)),
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
                params: params,
                tx: Some(tx),
            }));
        }

        rx.await.unwrap();
        Ok(())
    }

    pub async fn events(&self) -> impl Stream<Item = E> {
        let (tx, rx) = mpsc::channel(128);
        {
            let mut subscriptions = self.subscriptions.lock().await;
            subscriptions.push(tx);
        }
        rx
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("Error form mission script: {0}")]
    Script(String),
    #[error("Failed to deserialize params: {0}")]
    DeserializeParams(#[source] serde_mlua::Error),
    #[error("Failed to deserialize result for method {method}: {err}\n{result}")]
    DeserializeResult {
        #[source]
        err: serde_mlua::Error,
        method: String,
        result: String,
    },
    #[error("Failed to serialize params: {0}")]
    SerializeParams(#[source] serde_mlua::Error),
}

impl<E> Clone for RPC<E> {
    fn clone(&self) -> Self {
        RPC {
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

    fn params<'lua>(&self, lua: &'lua Lua) -> Result<Option<Value<'lua>>, serde_mlua::Error> {
        self.params
            .as_ref()
            .map(|params| serde_mlua::to_value(lua, params))
            .transpose()
    }

    fn success(&mut self, value: &Value) -> Result<(), serde_mlua::Error> {
        if let Some(tx) = self.tx.take() {
            let res = serde_mlua::from_value(value.clone())?;
            // log::debug!("Received: {:#?}", res);
            let _ = tx.send(Response::Success(res));
        }

        Ok(())
    }

    fn error(&mut self, error: String) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Response::Error(error));
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response<R> {
    Success(R),
    Error(String),
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
