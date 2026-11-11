#!/usr/bin/env python3
"""Small X11 desktop CLI and newline-delimited stdio MCP server."""
import argparse
import base64
from contextlib import contextmanager
import fcntl
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

TOOLS = {
    'screenshot': ('Capture the shared desktop as PNG.', {}),
    'click': ('Click a screen coordinate.', {'x': 'integer', 'y': 'integer', 'button': 'integer'}),
    'type': ('Type literal UTF-8 text.', {'text': 'string'}),
    'key': ('Press an xdotool key chord, e.g. ctrl+l.', {'key': 'string'}),
    'scroll': ('Scroll up or down a number of steps.', {'direction': 'string', 'steps': 'integer'}),
    'clipboard': ('Read the clipboard text.', {}),
}

def run(argv, **kwargs):
    return subprocess.run(argv, check=True, capture_output=True, timeout=20, **kwargs)

def integer(args, key, low, high, default=None):
    value = args.get(key, default)
    if type(value) is not int or not low <= value <= high:
        raise ValueError(f'{key} must be an integer from {low} to {high}')
    return value

def textarg(args, key, limit=16384):
    value = args.get(key)
    if not isinstance(value, str) or not value or len(value) > limit or '\0' in value:
        raise ValueError(f'{key} must be nonempty text, at most {limit} characters, without NUL')
    return value

@contextmanager
def desktop_lock(folder):
    folder.mkdir(parents=True, exist_ok=True, mode=0o700)
    with (folder / 'desktop.lock').open('a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        yield

def perform(name, args):
    if not isinstance(name, str) or name not in TOOLS or not isinstance(args, dict):
        raise ValueError('Unknown desktop tool or invalid arguments')
    if set(args) - set(TOOLS[name][1]):
        raise ValueError('Unexpected tool argument')
    folder = Path(os.environ.get('XDG_RUNTIME_DIR', f'/tmp/devbox-{os.getuid()}'))
    with desktop_lock(folder):
        if name in {'click', 'type', 'key', 'scroll'} and (folder / 'desktop.paused').exists():
            raise ValueError('Desktop input paused for human takeover')
        if name == 'screenshot':
            with tempfile.TemporaryDirectory(prefix='devbox-shot-') as temp:
                path = str(Path(temp) / 'screen.png')
                run(['scrot', path])
                return {'type': 'image', 'mimeType': 'image/png', 'data': base64.b64encode(Path(path).read_bytes()).decode()}
        if name == 'click':
            width, height = map(int, run(['xdotool', 'getdisplaygeometry']).stdout.split())
            x = integer(args, 'x', 0, width - 1)
            y = integer(args, 'y', 0, height - 1)
            button = integer(args, 'button', 1, 3, 1)
            run(['xdotool', 'mousemove', '--sync', str(x), str(y), 'click', str(button)])
        elif name == 'type':
            run(['xdotool', 'type', '--clearmodifiers', '--delay', '1', '--', textarg(args, 'text')])
        elif name == 'key':
            key = textarg(args, 'key', 128)
            if key.startswith('-') or any(c.isspace() for c in key):
                raise ValueError('Invalid key chord')
            run(['xdotool', 'key', '--clearmodifiers', key])
        elif name == 'scroll':
            direction = args.get('direction')
            if direction not in {'up', 'down'}:
                raise ValueError('direction must be up or down')
            steps = integer(args, 'steps', 1, 50, 3)
            run(['xdotool', 'click', '--repeat', str(steps), '--delay', '50', '4' if direction == 'up' else '5'])
        elif name == 'clipboard':
            value = run(['xclip', '-selection', 'clipboard', '-o']).stdout.decode('utf-8', errors='replace')
            return {'type': 'text', 'text': value[:65536]}
        return {'type': 'text', 'text': 'ok'}

def handle(request):
    if not isinstance(request, dict):
        return {'jsonrpc': '2.0', 'id': None, 'error': {'code': -32600, 'message': 'Invalid request'}}
    if 'id' not in request:
        return None
    reply = {'jsonrpc': '2.0', 'id': request['id']}
    method = request.get('method')
    params = request.get('params', {})
    if not isinstance(params, dict):
        return {**reply, 'error': {'code': -32602, 'message': 'params must be an object'}}
    if method == 'initialize':
        requested = params.get('protocolVersion')
        version = requested if requested in {'2024-11-05', '2025-03-26', '2025-06-18'} else '2024-11-05'
        result = {'protocolVersion': version, 'capabilities': {'tools': {}},
                  'serverInfo': {'name': 'devbox-desktop', 'version': '0.1.0'}}
    elif method == 'ping':
        result = {}
    elif method == 'tools/list':
        result = {'tools': [{'name': name, 'description': desc,
            'inputSchema': {'type': 'object', 'properties': {k: {'type': t} for k, t in props.items()},
                            'required': [k for k in props if k not in {'button', 'steps'}], 'additionalProperties': False}}
            for name, (desc, props) in TOOLS.items()]}
    elif method == 'tools/call':
        try:
            result = {'content': [perform(params.get('name'), params.get('arguments', {}))]}
        except (ValueError, OSError, subprocess.SubprocessError) as exc:
            result = {'isError': True, 'content': [{'type': 'text', 'text': str(exc)}]}
    else:
        return {**reply, 'error': {'code': -32601, 'message': 'Method not found'}}
    return {**reply, 'result': result}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--mcp', action='store_true')
    parser.add_argument('command', nargs='?', choices=[*TOOLS, 'pause', 'resume'])
    parser.add_argument('--args', default='{}', help='JSON object for tool arguments')
    parser.add_argument('--output', help='PNG path for CLI screenshot')
    args = parser.parse_args()
    if args.mcp:
        for line in sys.stdin:
            try:
                result = handle(json.loads(line))
            except json.JSONDecodeError:
                result = {'jsonrpc': '2.0', 'id': None, 'error': {'code': -32700, 'message': 'Parse error'}}
            if result is not None:
                print(json.dumps(result), flush=True)
        return
    if args.command in {'pause', 'resume'}:
        folder = Path(os.environ.get('XDG_RUNTIME_DIR', f'/tmp/devbox-{os.getuid()}'))
        with desktop_lock(folder):
            marker = folder / 'desktop.paused'
            marker.touch(mode=0o600) if args.command == 'pause' else marker.unlink(missing_ok=True)
        print(args.command)
        return
    if not args.command:
        parser.error('provide a command or --mcp')
    try:
        result = perform(args.command, json.loads(args.args))
        if result['type'] == 'image' and args.output:
            Path(args.output).write_bytes(base64.b64decode(result['data']))
            print(args.output)
        else:
            print(json.dumps(result))
    except (ValueError, OSError, subprocess.SubprocessError) as exc:
        parser.exit(1, f'{exc}\n')

if __name__ == '__main__':
    main()
