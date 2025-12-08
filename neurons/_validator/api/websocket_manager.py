from __future__ import annotations
from fastapi import WebSocket
import logging
from starlette.websockets import WebSocketState


class WebSocketManager:
    def __init__(self):
        self.active_connections: set[WebSocket] = set()

    async def connect(self, websocket: WebSocket) -> None:
        await websocket.accept()
        self.active_connections.add(websocket)

    async def disconnect(self, websocket: WebSocket) -> None:
        if websocket in self.active_connections:
            try:
                if websocket.application_state != WebSocketState.DISCONNECTED:
                    await websocket.close()
            except RuntimeError as e:
                if "Unexpected ASGI message 'websocket.close'" in str(e):
                    logging.info(
                        f"WebSocket close attempt on already closing/closed connection: {e}"
                    )
                else:
                    logging.error(
                        f"Unexpected RuntimeError during websocket close: {e}"
                    )
                    raise
            except Exception as e:
                logging.error(f"Unexpected error during websocket.close(): {e}")
            finally:
                self.active_connections.discard(websocket)
        else:
            logging.debug(
                f"Attempted to disconnect websocket not in active_connections: {websocket}"
            )

    async def close_all(self) -> None:
        for connection in self.active_connections.copy():
            await self.disconnect(connection)
