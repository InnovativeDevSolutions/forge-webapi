/*
    Forge Web API — /health check from Arma 3 (SQF)
    =================================================
    Calls GET /health on a running forge-webapi stack
    (default: http://127.0.0.1:8080) using Arma 3's WebAPI.

    Notes:
    - `webRequest` takes a HashMap and returns a promise (script handle).
      `waitUntil` suspends until the promise resolves; the result is an
      array of [request, result], where result is a HashMap with the keys
      "status" (HTTP status code) and "body".
      See https://wiki.arma3.io/commands/webRequest.html
    - The "origin" key sets the Origin header. Arma forces it to start with
      "arma://" — here "forge" becomes `Origin: arma://forge`. The API
      echoes that exact Origin back in Access-Control-Allow-Origin, and
      answers the CORS preflight the engine sends before the request.
    - "method" defaults to "GET", so it is omitted here.

    Run this from the debug console or a client-side script while the
    stack is up (`docker compose up` in this repo).
*/

private _apiBase = "http://127.0.0.1:8080";

[_apiBase] spawn {
    params ["_apiBase"];

    private _handle = webRequest #{
        "type": "http",
        "origin": "forge",
        "url": _apiBase + "/health"
    };

    private _result = waitUntil _handle;

    _result params ["_request", "_result"];

    private _status = _result get "status";
    private _body = _result get "body";

    systemChat format ["[Forge API] /health -> HTTP %1: %2", _status, _body];
};
