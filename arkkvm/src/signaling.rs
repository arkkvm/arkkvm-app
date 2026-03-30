use serde::{Deserialize, Serialize};
use socketioxide::extract::{Data, SocketRef};
use socketioxide::socket::DisconnectReason;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SignalPayload {
    to: String,
    from: String,
    signal: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct IceCandidatePayload {
    session_id: String,
    candidate: String,
}

/// Handles socket.io connections and signaling logic.
pub async fn on_connect(socket: SocketRef) {
    info!("[sid={}] connected", socket.id);

    socket.on("join", |socket: SocketRef, Data::<String>(room)| async move {
        info!("[sid={}] joining room {}", socket.id, room);
        socket.leave_all(); // Ensure the socket is only in one room at a time
        socket.join(room.clone());
        // Notify others in the room
        let id = socket.id.to_string();
        socket.to(room).emit("user-joined", &id).await.ok();
    });

    socket.on("signal", |socket: SocketRef, Data::<SignalPayload>(payload)| async move {
        info!("[sid={}] forwarding signal to {}", socket.id, payload.to);
        socket.broadcast().emit("signal", &payload).await.ok();
    });

    socket.on(
        "ice-candidate",
        |socket: SocketRef, Data::<IceCandidatePayload>(payload)| async move {
            info!("[sid={}] received ICE candidate for session {}", socket.id, payload.session_id);

            // Forward ICE candidate to the specific session
            let session_room = format!("session_{}", payload.session_id);
            socket.to(session_room).emit("ice-candidate", &payload).await.ok();
        },
    );

    socket.on_disconnect(|socket: SocketRef, reason: DisconnectReason| async move {
        info!("[sid={}] disconnected, reason: {:?}", socket.id, reason);
        // Notify other users in the rooms this socket was in
        let rooms = socket.rooms();
        for room in rooms {
            if room != socket.id.to_string() {
                let id = socket.id.to_string();
                socket.to(room).emit("user-left", &id).await.ok();
            }
        }
    });
}
