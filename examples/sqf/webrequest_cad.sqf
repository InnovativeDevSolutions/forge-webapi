/*
    Forge Web API — CAD board reads from Arma 3 (SQF)
    ==================================================
    Fetches the CAD board from a running forge-webapi stack
    (default: http://127.0.0.1:8080) using Arma 3's WebAPI:
      - available contracts: GET /cad/contracts?available=true
      - groups:              GET /cad/groups

    Each contract in the response carries its availability state
    ("available": true/false, "assignmentState": "unassigned" /
    "assigned" / "accepted" / "active") and, when assigned, an
    "assignment" object with the assigned group (groupId,
    groupCallsign), who assigned it (assignedByUid / assignedByName),
    when (assignedAt), the dispatcher note, and whether the group
    accepted it ("accepted" / "acceptedAt").

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
    - Group status updates (PUT /cad/groups/<id>/status) are issued by the
      dispatcher web UI / server bridge as a JSON PUT — the wiki documents
      no request-body key for `webRequest`, so they are not shown here.

    Run this from the debug console or a client-side script while the
    stack is up (`docker compose up` in this repo).
*/

private _apiBase = "http://127.0.0.1:8080";

[_apiBase] spawn {
    params ["_apiBase"];

    private _get = {
        params ["_apiBase", "_path"];
        private _handle = webRequest #{
            "type": "http",
            "origin": "forge",
            "url": _apiBase + _path
        };
        private _result = waitUntil _handle;
        _result params ["_request", "_result"];
        [_result get "status", _result get "body"]
    };

    private _contracts = [_apiBase, "/cad/contracts?available=true"] call _get;
    _contracts params ["_contractsStatus", "_contractsBody"];
    systemChat format [
        "[Forge API] available contracts -> HTTP %1: %2",
        _contractsStatus,
        _contractsBody
    ];

    private _groups = [_apiBase, "/cad/groups"] call _get;
    _groups params ["_groupsStatus", "_groupsBody"];
    systemChat format [
        "[Forge API] groups -> HTTP %1: %2",
        _groupsStatus,
        _groupsBody
    ];
};
