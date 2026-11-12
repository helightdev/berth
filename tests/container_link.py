#!/usr/bin/env python3
"""Reach the berthd of a running container from outside, the way berth does:
HTTP over the standard input and output of `docker exec -i CONTAINER berthd connect`.

    python3 tests/container_link.py CONTAINER

Starts a session and runs a task, and fails when either does not end well.
Not a unit test: run it against the built image, started with docker compose.
"""
import base64
import http.client
import json
import subprocess
import sys
import time


class Pipe:
    """Enough of a socket for http.client to read an answer from a pipe."""

    def __init__(self, stream):
        self.stream = stream

    def makefile(self, *_args, **_kwargs):
        return self.stream


def call(container, method, path, body=None):
    data = b'' if body is None else json.dumps(body).encode()
    head = (f'{method} {path} HTTP/1.1\r\nHost: berth\r\nConnection: close\r\n'
            f'Content-Type: application/json\r\nContent-Length: {len(data)}\r\n\r\n')
    link = subprocess.Popen(['docker', 'exec', '-i', container, 'berthd', 'connect'],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        link.stdin.write(head.encode() + data)
        link.stdin.flush()
        answer = http.client.HTTPResponse(Pipe(link.stdout), method=method)
        answer.begin()
        text = answer.read()
        if answer.status >= 400:
            raise RuntimeError(f'{method} {path}: {answer.status} {text.decode(errors="replace")}')
        return json.loads(text or b'null')
    except (http.client.HTTPException, ConnectionError) as error:
        said = link.stderr.read().decode(errors='replace').strip() if link.poll() is not None else ''
        raise RuntimeError(f'{method} {path}: {said or error!r}') from error
    finally:
        link.kill()
        link.wait()


def main():
    container = sys.argv[1]
    for attempt in range(60):
        try:
            version = call(container, 'GET', '/v1/version')
            break
        except RuntimeError as error:
            if attempt == 59:
                sys.exit(f'berthd did not answer: {error}')
            time.sleep(1)
    print(f'berthd {version["version"]}, api {version["api"]}, {version.get("mode", "")} mode')

    session = call(container, 'POST', '/v1/sessions?wait=30s',
                   {'kind': 'process', 'command': 'echo hi; echo problem >&2; exit 3'})
    out = base64.b64decode(session.get('stdout_b64') or '').decode()
    err = base64.b64decode(session.get('stderr_b64') or '').decode()
    if (session.get('exit_code'), out, err) != (3, 'hi\n', 'problem\n'):
        sys.exit(f'Unexpected session result: {session}')
    print(f'session {session["id"]}: output and exit code as expected')

    names = [task['name'] for task in call(container, 'GET', '/v1/tasks')]
    if 'sign-in:status' not in names:
        sys.exit(f'No sign-in:status among the tasks: {names}')
    task = call(container, 'POST', '/v1/tasks/sign-in:status?wait=120s', {})
    if task.get('exit_code') != 0:
        sys.exit(f'Task sign-in:status failed: {task}')
    print(f'task sign-in:status: ran as session {task["id"]}, steps {[step["name"] for step in task.get("steps", [])]}')

    refused = None
    try:
        call(container, 'POST', '/v1/sessions?wait=10s', {'kind': 'process', 'command': 'id', 'user': 'root'})
    except RuntimeError as error:
        refused = error
    if refused is None:
        sys.exit('A session as root was not refused in single-user mode')
    print('a session as another user is refused')


if __name__ == '__main__':
    main()
