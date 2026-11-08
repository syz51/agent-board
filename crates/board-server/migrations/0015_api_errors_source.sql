-- Who answered a recorded API error: the server itself (`server`), or a client that refused
-- the request before sending it and reported that to `POST /v1/errors` (`client`).
ALTER TABLE api_errors ADD COLUMN source TEXT NOT NULL DEFAULT 'server';
