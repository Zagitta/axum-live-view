use crate::{
    html::{self, Diff},
    liveview::liveview_local_topic,
    pubsub::PubSub,
    LiveViewManager, PubSubExt,
};
use axum::{
    extract::ws::{self, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use futures_util::{stream::BoxStream, StreamExt};
use serde::Deserialize;
use serde_json::{from_value, json, Value};
use tokio_stream::StreamMap;
use uuid::Uuid;

pub(crate) fn routes<B>() -> Router<B>
where
    B: Send + 'static,
{
    Router::new().route("/live", get(ws))
}

async fn ws(upgrade: WebSocketUpgrade, live: LiveViewManager) -> impl IntoResponse {
    upgrade.on_upgrade(move |socket| handle_socket(socket, live.pubsub))
}

#[derive(Default)]
struct SocketState {
    diff_streams: StreamMap<Uuid, BoxStream<'static, Diff>>,
}

async fn handle_socket<P>(mut socket: WebSocket, pubsub: P)
where
    P: PubSub,
{
    let mut state = SocketState::default();

    loop {
        tokio::select! {
            Some(msg) = socket.recv() => {
                match msg {
                    Ok(msg) => {
                        if let Some((liveview_id, html)) = handle_message_from_socket(msg, &pubsub, &mut state).await {
                            if send_message_to_socket(&mut socket, liveview_id, INITIAL_RENDER_TOPIC, html).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        tracing::trace!(%err, "error from socket");
                        break;
                    }
                }
            }

            Some((liveview_id, diff)) = state.diff_streams.next() => {
                if send_message_to_socket(&mut socket, liveview_id, RENDERED_TOPIC, diff).await.is_err() {
                    break;
                }
            }
        }
    }

    let liveview_ids = state
        .diff_streams
        .iter()
        .map(|(id, _)| *id)
        .collect::<Vec<_>>();

    for liveview_id in liveview_ids {
        let _ = pubsub
            .broadcast(
                &liveview_local_topic(liveview_id, "socket-disconnected"),
                (),
            )
            .await;
    }
}

const INITIAL_RENDER_TOPIC: &str = "i";
const RENDERED_TOPIC: &str = "r";

async fn send_message_to_socket<T>(
    socket: &mut WebSocket,
    liveview_id: Uuid,
    topic: &'static str,
    msg: T,
) -> Result<(), axum::Error>
where
    T: serde::Serialize,
{
    let msg = json!([liveview_id, topic, msg,]);
    let msg = serde_json::to_string(&msg).unwrap();
    tracing::trace!(%msg, "sending message to websocket");

    socket.send(ws::Message::Text(msg)).await
}

async fn handle_message_from_socket<P>(
    msg: ws::Message,
    pubsub: &P,
    state: &mut SocketState,
) -> Option<(Uuid, html::Serialized)>
where
    P: PubSub,
{
    #[derive(Debug, Deserialize)]
    struct RawMessage {
        liveview_id: Uuid,
        topic: String,
        data: Value,
    }

    impl TryFrom<RawMessage> for Message {
        type Error = anyhow::Error;

        fn try_from(value: RawMessage) -> Result<Self, Self::Error> {
            let RawMessage {
                topic,
                data,
                liveview_id: _,
            } = value;

            match &*topic {
                "axum/mount-liveview" => Ok(Message::Mount),
                "axum/live-click" => Ok(Message::LiveClick(from_value(data)?)),
                "axum/live-input" => Ok(Message::LiveInput(from_value(data)?)),
                other => {
                    anyhow::bail!("unknown message topic: {:?}", other)
                }
            }
        }
    }

    #[derive(Debug)]
    enum Message {
        Mount,
        LiveClick(LiveClick),
        LiveInput(LiveInput),
    }

    #[derive(Debug, Deserialize)]
    struct LiveClick {
        #[serde(rename = "e")]
        event_name: String,
        #[serde(rename = "d")]
        additional_data: Option<Value>,
    }

    #[derive(Debug, Deserialize)]
    struct LiveInput {
        #[serde(rename = "e")]
        event_name: String,
        #[serde(rename = "v")]
        value: String,
    }

    macro_rules! try_ {
        ($expr:expr, $pattern:path $(,)?) => {
            match $expr {
                $pattern(inner) => inner,
                other => {
                    tracing::error!(?other);
                    return None;
                }
            }
        };
    }

    let text = try_!(msg, ws::Message::Text);
    let msg: RawMessage = try_!(serde_json::from_str(&text), Ok);
    let liveview_id = msg.liveview_id;
    let msg = try_!(Message::try_from(msg), Ok);

    tracing::trace!(?msg, "received message from websocket");

    match msg {
        Message::Mount => {
            let mut initial_render_stream = pubsub
                .subscribe::<Json<html::Serialized>>(&liveview_local_topic(
                    liveview_id,
                    "initial-render",
                ))
                .await;

            try_!(
                pubsub
                    .broadcast(&liveview_local_topic(liveview_id, "mounted"), ())
                    .await,
                Ok,
            );

            let Json(msg) = try_!(initial_render_stream.next().await, Some);

            let diff_stream = pubsub
                .subscribe::<Json<Diff>>(&liveview_local_topic(liveview_id, "rendered"))
                .await
                .map(|Json(diff)| diff);

            state
                .diff_streams
                .insert(liveview_id, Box::pin(diff_stream));

            Some((liveview_id, msg))
        }
        Message::LiveClick(LiveClick {
            event_name,
            additional_data,
        }) => {
            let topic = liveview_local_topic(liveview_id, &event_name);
            if let Some(additional_data) = additional_data {
                try_!(
                    pubsub.broadcast(&topic, axum::Json(additional_data)).await,
                    Ok
                );
            } else {
                try_!(pubsub.broadcast(&topic, ()).await, Ok);
            }

            None
        }
        Message::LiveInput(LiveInput { event_name, value }) => {
            let topic = liveview_local_topic(liveview_id, &event_name);
            try_!(pubsub.broadcast(&topic, value).await, Ok);

            None
        }
    }
}