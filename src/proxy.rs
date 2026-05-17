//! Reverse-proxy `/claws/<name>/proxy/*` to the claw's gateway, injecting
//! the per-claw bearer for HTTP (`Authorization`) and WebSocket
//! (`Sec-WebSocket-Protocol: bearer.<token>`) requests.
