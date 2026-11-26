"""Offline fixed Loom evaluation. Run with Python 3 -I; standard library only.

No sample code is executed. Every Loom process has both cwd and --root under
ALLOWED_WORK_ROOT. Input copies exclude .loom and retain a real .git directory.
"""
import argparse
import copy
import csv
import ctypes
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import re
import shutil
import statistics
import subprocess
import sys
import threading
import time
import uuid
from datetime import datetime, timezone

HERE = Path(__file__).resolve().parent
# Persistent dedicated root; --work never selects an arbitrary disk root.
EVAL_HOME = Path(os.environ.get('LOOM_EVAL_HOME', str(Path.home() / '.cache' / 'loom-eval'))).expanduser()
ALLOWED_WORK_ROOT = EVAL_HOME / 'work'
ULID = re.compile(r'\b[0-9A-HJKMNP-TV-Z]{26}\b')
REPARSE = 0x400
TIMEOUT = 180


def digest_bytes(data):
    return hashlib.sha256(data).hexdigest()


def digest_file(path):
    h = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            h.update(chunk)
    return h.hexdigest()


def load_json(path):
    return json.loads(Path(path).read_text(encoding='utf-8'))


def save_json(path, value):
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(value, ensure_ascii=False, indent=2) + '\n', encoding='utf-8', newline='\n')


def is_link(path):
    stat = Path(path).lstat()
    return Path(path).is_symlink() or bool(getattr(stat, 'st_file_attributes', 0) & REPARSE)


def guard(path, allow_root=False):
    """Reject root/sibling escapes, traversal and links, including in the work root."""
    if not EVAL_HOME.is_absolute() or EVAL_HOME.name != 'loom-eval':
        raise ValueError('LOOM_EVAL_HOME must be an absolute dedicated directory named loom-eval')
    if '..' in Path(path).parts:
        raise ValueError('Refuse traversal in work path')
    path = Path(os.path.abspath(path))
    for candidate in (path, ALLOWED_WORK_ROOT):
        for ancestor in [candidate, *candidate.parents]:
            if (ancestor.exists() or ancestor.is_symlink()) and is_link(ancestor):
                raise ValueError('Refuse link/junction in work path')
    root = ALLOWED_WORK_ROOT.resolve()
    resolved = path.resolve()
    if not resolved.is_relative_to(root) or (resolved == root and not allow_root):
        raise ValueError('Work path must be a strict child of LOOM_EVAL_HOME/work')
    return resolved


def refuse_production(exe):
    """Protect PATH installations, usual install directories and an explicit install."""
    names = ('loom', 'loom.exe')
    home = Path.home()
    install_dirs = [home / '.cargo' / 'bin', home / '.local' / 'bin',
                    Path('/usr/local/bin'), Path('/usr/bin')]
    if os.environ.get('CARGO_HOME'):
        install_dirs.append(Path(os.environ['CARGO_HOME']) / 'bin')
    for variable in ('LOCALAPPDATA', 'PROGRAMFILES', 'PROGRAMFILES(X86)'):
        if os.environ.get(variable):
            install_dirs.append(Path(os.environ[variable]) / 'Loom')
            if variable == 'LOCALAPPDATA':
                install_dirs.append(Path(os.environ[variable]) / 'Programs' / 'Loom')
    protected = [directory / name for directory in install_dirs for name in names]
    protected += [Path(found) for name in names if (found := shutil.which(name))]
    if os.environ.get('LOOM_EVAL_PRODUCTION_EXE'):
        protected.append(Path(os.environ['LOOM_EVAL_PRODUCTION_EXE']).expanduser())
    exe = Path(exe).resolve(strict=True)
    for installed in protected:
        if exe == installed.resolve() or (installed.is_file() and exe.samefile(installed)):
            raise ValueError('Refuse installed production Loom; use a separate trusted evaluation build')
    return exe


def child(root, relative):
    relative = str(relative).replace('\\', '/')
    if Path(relative).is_absolute() or ':' in relative or '..' in Path(relative).parts:
        raise ValueError('Unsafe relative path: ' + relative)
    p = guard(Path(root) / relative)
    if not p.is_relative_to(Path(root).resolve()):
        raise ValueError('Path escaped fixture: ' + relative)
    return p


def environment():
    env = dict(os.environ)
    for name in list(env):
        if name.startswith('LOOM_'):
            del env[name]
    # 评测不读这台机器的用户配置：库位置和模型设置都按默认（库在样本副本里，模型关闭）。
    config = guard(ALLOWED_WORK_ROOT / 'no-user-config.toml')
    if config.exists() or config.is_symlink():
        raise ValueError('Evaluation config sentinel must not exist')
    env['LOOM_CONFIG'] = str(config)
    env['NO_COLOR'] = '1'
    env['GIT_CONFIG_NOSYSTEM'] = '1'
    env['GIT_CONFIG_GLOBAL'] = os.devnull
    env['GIT_OPTIONAL_LOCKS'] = '0'
    env['GIT_NO_LAZY_FETCH'] = '1'
    return env


class Counters(ctypes.Structure):
    _fields_ = [('cb', ctypes.c_ulong), ('PageFaultCount', ctypes.c_ulong),
                ('PeakWorkingSetSize', ctypes.c_size_t), ('WorkingSetSize', ctypes.c_size_t),
                ('QuotaPeakPagedPoolUsage', ctypes.c_size_t), ('QuotaPagedPoolUsage', ctypes.c_size_t),
                ('QuotaPeakNonPagedPoolUsage', ctypes.c_size_t), ('QuotaNonPagedPoolUsage', ctypes.c_size_t),
                ('PagefileUsage', ctypes.c_size_t), ('PeakPagefileUsage', ctypes.c_size_t)]


def memory(proc):
    if os.name != 'nt':
        return {'working_set_bytes': None, 'peak_working_set_bytes': None, 'memory_error': 'Windows psapi required'}
    data = Counters()
    data.cb = ctypes.sizeof(data)
    get = ctypes.WinDLL('psapi', use_last_error=True).GetProcessMemoryInfo
    get.argtypes = [ctypes.c_void_p, ctypes.POINTER(Counters), ctypes.c_ulong]
    get.restype = ctypes.c_int
    if not get(int(proc._handle), ctypes.byref(data), data.cb):
        return {'working_set_bytes': None, 'peak_working_set_bytes': None, 'memory_error': str(ctypes.get_last_error())}
    return {'working_set_bytes': data.WorkingSetSize, 'peak_working_set_bytes': data.PeakWorkingSetSize}


def invoke(exe, root, args, timeout=TIMEOUT):
    root = guard(root)
    if any(x in args for x in ('--root', '--db')):
        raise ValueError('Root/database overrides are forbidden in evaluated commands')
    command = [str(exe), '--root', str(root), *map(str, args)]
    start = time.perf_counter()
    proc = subprocess.Popen(command, cwd=root, env=environment(), stdin=subprocess.DEVNULL,
                            stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    peak = 0
    timed_out = False
    try:
        while True:
            observed = memory(proc)
            peak = max(peak, observed.get('peak_working_set_bytes') or 0)
            try:
                out, err = proc.communicate(timeout=0.005)
                break
            except subprocess.TimeoutExpired:
                if time.perf_counter() - start > timeout:
                    timed_out = True
                    proc.kill()
                    out, err = proc.communicate()
                    break
        elapsed = (time.perf_counter() - start) * 1000
        after = memory(proc)
        peak = max(peak, after.get('peak_working_set_bytes') or 0)
        return {'transport': 'cli', 'args': list(map(str, args)), 'returncode': proc.returncode,
                'error': bool(proc.returncode) or timed_out, 'timed_out': timed_out,
                'text': out.decode('utf-8', 'strict'), 'stderr': err.decode('utf-8', 'replace'),
                'stdout_bytes': len(out), 'stderr_bytes': len(err), 'answer_bytes': len(out) + len(err),
                'wire_bytes': len(out) + len(err), 'ms': elapsed,
                'peak_working_set_bytes': peak or None,
                'memory_error': None if peak else after.get('memory_error', 'no memory sample')}
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()


def checked(exe, root, args):
    result = invoke(exe, root, args)
    if result['error']:
        raise RuntimeError('Setup failed: ' + repr(args) + '\n' + result['text'] + result['stderr'])
    return result


class MCP:
    def __init__(self, exe, root):
        self.root = guard(root)
        self.proc = subprocess.Popen([str(exe), '--root', str(self.root), 'serve'], cwd=self.root,
                                     env=environment(), stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.PIPE)
        self.lines = queue.Queue()
        self.stderr = bytearray()
        self.next_id = 0
        self.protocol_bytes = 0
        self.protocol_requests = 0
        self.transcript = []
        threading.Thread(target=self._pump, daemon=True).start()
        threading.Thread(target=self._pump_stderr, daemon=True).start()
        try:
            self.send('initialize', {'protocolVersion': '2025-06-18', 'capabilities': {},
                                    'clientInfo': {'name': 'loom-fixed-eval', 'version': '1'}})
            self.send('notifications/initialized', notify=True)
        except BaseException:
            self.close()
            raise

    def _pump(self):
        for raw in self.proc.stdout:
            self.lines.put(raw)
        self.lines.put(None)

    def _pump_stderr(self):
        for raw in iter(lambda: self.proc.stderr.read(4096), b''):
            self.stderr.extend(raw)

    def send(self, method, params=None, notify=False):
        msg = {'jsonrpc': '2.0', 'method': method}
        if params is not None:
            msg['params'] = params
        if not notify:
            self.next_id += 1
            msg['id'] = self.next_id
            self.protocol_requests += 1
        data = json.dumps(msg, ensure_ascii=False, separators=(',', ':')).encode('utf-8') + b'\n'
        self.proc.stdin.write(data)
        self.proc.stdin.flush()
        self.transcript.append({'sent': msg})
        if notify:
            return None, 0
        deadline = time.monotonic() + TIMEOUT
        while True:
            raw = self.lines.get(timeout=max(0.01, deadline - time.monotonic()))
            if raw is None:
                raise RuntimeError('MCP exited early: ' + self.stderr.decode('utf-8', 'replace'))
            self.protocol_bytes += len(raw)
            reply = json.loads(raw.decode('utf-8'))
            self.transcript.append({'received': reply, 'raw_utf8': raw.decode('utf-8')})
            if reply.get('id') == msg['id']:
                return reply, len(raw)
            if time.monotonic() >= deadline:
                raise TimeoutError('MCP request timeout')

    def call(self, name, args):
        start = time.perf_counter()
        reply, wire = self.send('tools/call', {'name': name, 'arguments': args})
        result = reply.get('result', {})
        text = '\n'.join(c.get('text', '') for c in result.get('content', []) if c.get('type') == 'text')
        failed = bool(result.get('isError')) or 'error' in reply
        if 'error' in reply:
            text = json.dumps(reply['error'], ensure_ascii=False)
        size = len(text.encode('utf-8'))
        return {'transport': 'mcp', 'tool': name, 'arguments': args, 'error': failed,
                'returncode': None, 'text': text, 'stderr': '', 'stdout_bytes': size,
                'stderr_bytes': 0, 'answer_bytes': size, 'wire_bytes': wire,
                'ms': (time.perf_counter() - start) * 1000, 'rpc_reply': reply}

    def close(self):
        if self.proc.stdin and not self.proc.stdin.closed:
            try:
                self.proc.stdin.close()
            except OSError:
                pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        return {'returncode': self.proc.returncode, 'stderr': self.stderr.decode('utf-8', 'replace'),
                'protocol_requests': self.protocol_requests, 'protocol_response_bytes': self.protocol_bytes,
                'transcript': self.transcript}


def source_inventory(root):
    root = Path(root).resolve()
    rows = []
    for parent, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d not in ('.git', '.loom'))
        for d in dirs:
            if is_link(Path(parent) / d):
                raise ValueError('Sample directory link is not permitted: ' + str(Path(parent) / d))
        for name in sorted(files):
            if name in ('.git', '.loom'):
                continue
            p = Path(parent) / name
            if is_link(p):
                raise ValueError('Sample file link is not permitted: ' + str(p))
            rows.append((p.relative_to(root).as_posix(), digest_file(p)))
    content = ''.join(p + '\0' + h + '\n' for p, h in sorted(rows)).encode('utf-8')
    return {'sha256': digest_bytes(content), 'files': len(rows)}


def read_git_head(root):
    """Read HEAD/refs without executing Git, hooks, config or sample programs."""
    git = Path(root) / '.git'
    if not git.is_dir() or is_link(git):
        raise ValueError('Expected a real .git directory, not a worktree pointer')
    head = (git / 'HEAD').read_text(encoding='ascii').strip()
    if not head.startswith('ref: '):
        return head
    ref = head[5:]
    if '..' in Path(ref).parts or not ref.startswith('refs/'):
        raise ValueError('Unsafe git HEAD reference')
    p = git / ref
    if p.exists():
        return p.read_text(encoding='ascii').strip()
    packed = git / 'packed-refs'
    if packed.exists():
        for line in packed.read_text(encoding='ascii').splitlines():
            if line and not line.startswith(('#', '^')):
                sha, name = line.split(' ', 1)
                if name == ref:
                    return sha
    raise ValueError('Cannot resolve HEAD reference ' + ref)


def copy_sample(source, destination):
    destination = guard(destination)
    if destination.exists():
        raise ValueError('Refuse to reuse a sample copy: ' + str(destination))
    source = Path(source).resolve()
    # Validate .git as well: copytree must never follow an untrusted link/junction.
    for parent, dirs, files in os.walk(source, followlinks=False):
        dirs[:] = [d for d in dirs if d != '.loom']
        for name in dirs + [f for f in files if f != '.loom']:
            if is_link(Path(parent) / name):
                raise ValueError('Refuse link while copying sample: ' + str(Path(parent) / name))
    shutil.copytree(source, destination, ignore=lambda _p, names: [n for n in names if n == '.loom'])
    return destination


def file_bytes(value):
    if isinstance(value, str):
        return value.encode('utf-8')
    if 'hex' in value:
        return bytes.fromhex(value['hex'])
    return (value['repeat'] * value['count'] + value.get('suffix', '')).encode('utf-8')


def write_files(root, files):
    for name, value in files.items():
        path = child(root, name)
        path.parent.mkdir(parents=True, exist_ok=True)
        if value is None:
            path.unlink()
        else:
            path.write_bytes(file_bytes(value))


def identifier(text, what):
    ids = ULID.findall(text)
    if not ids:
        raise RuntimeError('Setup did not return ' + what + ': ' + text)
    return ids[0]


def prepare(exe, root, fixture):
    root = guard(root)
    root.mkdir(parents=True, exist_ok=False)
    write_files(root, fixture.get('files', {}))
    generated = fixture.get('generate')
    if generated:
        for n in range(generated['callers']):
            name = ('a_very_long_but_fixed_importer_path_%03d.ts' if generated.get('long') else 'caller_%03d.ts') % n
            child(root, name).write_text("import './" + generated['target'] + "';\n", encoding='utf-8', newline='\n')
    setup = []
    init = checked(exe, root, ['init'])
    setup.append(init)
    state = {'project_id': identifier(init['text'], 'project id'), 'index_reports': [], 'setup': setup}
    if fixture.get('index', True):
        result = checked(exe, root, ['index', '--json'])
        state['index_reports'].append(json.loads(result['text']))
        setup.append(result)
    if fixture.get('knowledge'):
        target = fixture.get('knowledge_target', 'target.ts')
        saved = checked(exe, root, ['remember', 'GOTCHA', '锁冲突时先退避',
                                  '因为同时写入会产生锁冲突，本地写入遇到锁冲突时先退避；不要马上重试。', '--anchor', target])
        setup.append(saved)
        state['memory_id'] = identifier(saved['text'], 'memory id')
        proposed = checked(exe, root, ['intent', 'propose', 'constraint', '离线红线',
                                     '因为此项目服务离线环境，保存记忆必须本地完成，不能要求网络。'])
        setup.append(proposed)
        pid = identifier(proposed['text'], 'intent id')
        state['intent_id'] = pid
        setup.append(checked(exe, root, ['intent', 'confirm', pid, '--yes']))
        sha = digest_file(child(root, target))
        setup.append(checked(exe, root, ['note', 'set', target, sha, '本地事务写入入口；只负责本地持久化与冲突退避。']))
        if fixture.get('archive'):
            setup.append(checked(exe, root, ['manage', 'archive', state['memory_id']]))
        if fixture.get('proposal'):
            setup.append(checked(exe, root, ['intent', 'propose', 'constraint', '尚待提议不要联网生效',
                                           '此项尚未人工确认，不能作为已经生效的约束。']))
    write_files(root, fixture.get('after', {}))
    if fixture.get('reindex'):
        result = checked(exe, root, ['index', '--json'])
        state['index_reports'].append(json.loads(result['text']))
        setup.append(result)
    return state


def cli_arguments(request):
    command = request['command']
    if command in ('explain', 'impact'):
        return [command, request['target'], '--depth', str(request.get('depth', 1))]
    if command == 'context':
        return ['context', request['target'], '--mode', request['mode'], '--question', request['question'],
                '--depth', str(request.get('depth', 1)), '--budget', str(request['budget'])]
    if command == 'recall':
        return ['recall', request.get('query', ''), '--top-k', str(request.get('top_k', 5))]
    if command == 'note':
        return ['note', request.get('action', 'get'), request['target']]
    return [command]


def mcp_arguments(request):
    command = request['command']
    if command in ('explain', 'impact'):
        return {'target': request['target'], 'depth': request.get('depth', 1)}
    if command == 'context':
        return {k: request[k] for k in ('target', 'mode', 'question', 'depth', 'budget') if k in request}
    if command == 'recall':
        return {'query': request.get('query', ''), 'top_k': request.get('top_k', 5), 'full': request.get('full', False)}
    return {}


def ask(exe, root, request):
    if request.get('transport', 'cli') == 'cli':
        return invoke(exe, root, cli_arguments(request)), None
    client = MCP(exe, root)
    try:
        answer = client.call(request['command'], mcp_arguments(request))
    finally:
        protocol = client.close()
    answer['session_ok'] = protocol['returncode'] == 0
    return answer, protocol


def substitute(value, state):
    if isinstance(value, str):
        if value.startswith('$') and value[1:] in state:
            return state[value[1:]]
        return value
    if isinstance(value, list):
        return [substitute(x, state) for x in value]
    if isinstance(value, dict):
        return {k: substitute(v, state) for k, v in value.items()}
    return value


def check_record(checks, name, ok, expected=None, actual=None):
    item = {'check': name, 'pass': bool(ok)}
    if not ok:
        item.update(expected=expected, actual=actual)
    checks.append(item)


def text_checks(facts, text, rules, checks):
    for value in facts.get('text_all', []):
        check_record(checks, 'text contains ' + value, value in text, value, text[:180])
    if facts.get('text_any'):
        check_record(checks, 'text contains one alternative', any(x in text for x in facts['text_any']), facts['text_any'])
    for value in facts.get('text_not', []):
        check_record(checks, 'text excludes ' + value, value not in text, 'absent')
    for pattern in facts.get('text_regex', []):
        check_record(checks, 'text regex ' + pattern, re.search(pattern, text, re.I) is not None, pattern)
    if facts.get('snapshot_caveat'):
        pattern = rules['snapshot_caveat_regex']
        check_record(checks, 'negative confined to managed snapshot', bool(re.search(pattern, text)), pattern)
    if facts.get('gap_text'):
        check_record(checks, 'coverage gap disclosed', bool(re.search(rules['gap_regex'], text)), rules['gap_regex'])
    for path in facts.get('changed_paths', []):
        lines = [line for line in text.splitlines() if path in line]
        ok = any(re.search('已变化|已改变|发生变化|changed|已消失', line, re.I) for line in lines)
        check_record(checks, 'changed path disclosed ' + path, ok, path + ' and changed on same line')
    if 'max_text_bytes' in facts:
        size = len(text.encode('utf-8'))
        check_record(checks, 'delivery byte limit', size <= facts['max_text_bytes'], facts['max_text_bytes'], size)
    if facts.get('whole_relation_lines'):
        lines = [line for line in text.splitlines() if '→' in line]
        pattern = r'^.+ → .+ · 第 [0-9]+ 行 · (?:静态确定|可能) · 第 [0-9]+ 跳$'
        check_record(checks, 'complete relation lines', bool(lines) and all(re.fullmatch(pattern, line) for line in lines), pattern, lines)
    if facts.get('omission_counts'):
        ok = any('省略' in line and re.search(r'(?:\d+\s*条?\s*关系|关系\s*\d+\s*条?)', line) for line in text.splitlines())
        check_record(checks, 'preview counts omitted relations', ok, '省略 + numeric relation count')
    if facts.get('top5_id'):
        ranked = [line for line in text.splitlines() if re.match(r'^\[[1-5]\]', line)]
        check_record(checks, 'expected memory in top five', any(facts['top5_id'] in line for line in ranked), facts['top5_id'], ranked)


def grade_facts(spec, answers, report, state, rules):
    facts = substitute(spec.get('facts', {}), state)
    checks = []
    primary = [a for a in answers if a.get('role') != 'diagnostic']
    text = '\n'.join(a['text'] + (a.get('stderr', '') if facts.get('expect_error') else '') for a in primary)
    if facts.get('expect_error'):
        check_record(checks, 'request refused', any(a['error'] for a in primary), True)
    else:
        check_record(checks, 'requests succeeded', bool(primary) and all(not a['error'] for a in answers), True)
    check_record(checks, 'MCP sessions closed normally', all(a.get('session_ok', True) for a in answers), True)
    text_checks(facts, text, rules, checks)
    if spec['kind'] == 'budget':
        minima = [int(n) for n in re.findall(r'(?:最低|至少|最少|minimum|requires?)[^0-9\\n]*([0-9]+)', text, re.I)]
        check_record(checks, 'minimum budget exceeds refused budget', any(n > spec['request']['budget'] for n in minima), '> ' + str(spec['request']['budget']), minima)
    if report is not None:
        for key in ('status', 'truncated'):
            if key in facts:
                check_record(checks, key, report.get(key) == facts[key], facts[key], report.get(key))
        relations = report.get('relations', [])
        rendered = []
        for line in text.splitlines():
            found = re.fullmatch(r'(.+) → (.+) · 第 ([0-9]+) 行 · (静态确定|可能) · 第 ([0-9]+) 跳', line)
            if found:
                source,target,number,confidence,hop = found.groups()
                row = {'source':source,'target':target,'line':int(number),'confidence':'exact' if confidence=='静态确定' else 'likely','hop':int(hop)}
                rendered.append(row)
                check_record(checks, 'rendered relation matches JSON', any(all(r.get(k)==v for k,v in row.items()) for r in relations), row)
        if facts.get('omission_counts'):
            counts = []
            for line in text.splitlines():
                if '省略' in line:
                    counts.extend(int(a or b) for a,b in re.findall(r'(?:([0-9]+)\s*条?\s*关系|关系\s*([0-9]+)\s*条?)', line))
            check_record(checks, 'omitted relation count equals JSON minus displayed lines', len(relations)-len(rendered) in counts, len(relations)-len(rendered), counts)
        for required in facts.get('relations', []):
            ok = any(all(r.get(k) == v for k, v in required.items()) for r in relations)
            check_record(checks, 'relation evidence', ok, required, relations if not ok else None)
        if 'relation_count' in facts:
            check_record(checks, 'relation count', len(relations) == facts['relation_count'], facts['relation_count'], len(relations))
        for issue in facts.get('issues', []):
            ok = any(all((set(v).issubset(set(r.get(k, []))) if k == 'candidates' else r.get(k) == v)
                         for k, v in issue.items()) for r in report.get('issues', []))
            check_record(checks, 'unresolved evidence', ok, issue, report.get('issues', []))
        coverage = report.get('coverage', {})
        for k, v in facts.get('coverage_min', {}).items():
            check_record(checks, 'coverage minimum ' + k, isinstance(coverage.get(k), int) and coverage[k] >= v, v, coverage.get(k))
        for k, v in facts.get('coverage_exact', {}).items():
            check_record(checks, 'coverage exact ' + k, coverage.get(k) == v, v, coverage.get(k))
    elif spec['kind'] == 'structure':
        check_record(checks, 'valid JSON diagnostic', False, 'QueryReport JSON')
    # SPEC 3.3: a negative statement is unsafe iff the WHOLE answer lacks a gap/caveat.
    unsafe = []
    if spec['kind'] in ('structure', 'task'):
        for index, answer in enumerate(primary):
            command = answer.get('tool') or (answer.get('args') or [''])[0]
            if command not in ('explain', 'impact', 'context'):
                continue
            text_one = answer['text']
            if re.search(rules['negative_regex'], text_one) and not re.search(rules['caveat_regex'], text_one):
                unsafe.append({'answer': index, 'excerpt': text_one[:160]})
        check_record(checks, 'no unsupported negative statement', not unsafe, 0, len(unsafe))
    return {'status': 'pass' if all(c['pass'] for c in checks) else 'fail', 'checks': checks,
            'false_negatives': unsafe}


def grade_contract(spec, report, state):
    if not spec.get('contract') or not report or report.get('contract', 1) < 2:
        return {'status': 'not_applicable', 'reason': 'QueryReport contract < 2 or non-structure question'}
    expected = spec['contract']
    checks = []
    conclusion = report.get('conclusion') or {}
    check_record(checks, 'conclusion.verdict', conclusion.get('verdict') == expected['verdict'], expected['verdict'], conclusion.get('verdict'))
    actual_gaps = conclusion.get('gaps')
    check_record(checks, 'conclusion.gaps exactly', isinstance(actual_gaps, list) and set(actual_gaps) == set(expected['gaps']), expected['gaps'], actual_gaps)
    verification = report.get('verification') or {}
    for collection, expected_key, identity in [('files', 'files', 'path'), ('inputs', 'inputs', 'kind')]:
        rows = verification.get(collection, [])
        for key, value in expected.get(expected_key, {}).items():
            check_record(checks, 'verification.' + collection + ':' + key,
                         any(r.get(identity) == key and r.get('state') == value for r in rows), value, rows)
    snap = report.get('snapshot')
    if expected.get('snapshot_null'):
        check_record(checks, 'snapshot is null', snap is None, None, snap)
    else:
        generation = snap.get('generation') if isinstance(snap, dict) else None
        check_record(checks, 'snapshot.generation positive integer', type(generation) is int and generation > 0, '> 0', generation)
        if state.get('index_reports'):
            check_record(checks, 'query generation equals last index', generation == state['index_reports'][-1].get('generation'), state['index_reports'][-1].get('generation'), generation)
    if expected.get('generation_advanced'):
        generations = [r.get('generation') for r in state['index_reports']]
        check_record(checks, 'successive index generations increase', len(generations) == 2 and all(type(g) is int for g in generations) and generations[1] > generations[0], 'two increasing generations', generations)
    if expected.get('verification_empty'):
        check_record(checks, 'no filesystem verification for absent/not_indexed', verification.get('files') == [] and verification.get('inputs') == [], 'empty files and inputs', verification)
    else:
        check_record(checks, 'verification has states', isinstance(verification.get('files'), list) and all(r.get('state') in ('same','changed','missing','unverifiable') for r in verification['files']), 'valid file states', verification)
        check_record(checks, 'verification.skipped integer', type(verification.get('skipped')) is int and verification['skipped'] >= 0, 'nonnegative int', verification.get('skipped'))
        involved = [report['target']]
        for row in report.get('relations', []):
            involved.extend([row['source'], row['target']])
        involved.extend(row['source'] for row in report.get('issues', []))
        involved = list(dict.fromkeys(involved))
        # Rust may add declaration-chain files. At least the target is always first;
        # fixture files have no such chains and must honor the exact 32-file bound.
        required = involved[:32] if not spec.get('sample') else involved[:1]
        observed = {row.get('path'):row.get('state') for row in verification.get('files', [])}
        for path in required:
            wanted = expected.get('files', {}).get(path, 'same')
            check_record(checks, 'verified involved file ' + path, observed.get(path) == wanted, wanted, observed.get(path))
        if not spec.get('sample'):
            check_record(checks, 'verification skipped matches 32-file bound', verification.get('skipped') == max(0,len(involved)-32), max(0,len(involved)-32), verification.get('skipped'))
    coverage = report.get('coverage') or {}
    if not expected.get('snapshot_null'):
        check_record(checks, 'coverage.known', coverage.get('known') is True, True, coverage.get('known'))
    for key, count in expected.get('uncovered', {}).items():
        observed = (coverage.get('uncovered') or {}).get(key, 0)
        check_record(checks, 'uncovered reason ' + key, observed == count, count, observed)
    if 'excluded_dirs_min' in expected:
        actual = coverage.get('excluded_dirs')
        check_record(checks, 'excluded directories counted', type(actual) is int and actual >= expected['excluded_dirs_min'], expected['excluded_dirs_min'], actual)
    if 'scope' in expected:
        check_record(checks, 'snapshot.scope', bool(snap) and snap.get('scope') == expected['scope'], expected['scope'], snap)
    return {'status': 'pass' if all(c['pass'] for c in checks) else 'fail', 'checks': checks}


def artifact_bundle(directory, stem, data):
    directory = guard(directory)
    directory.mkdir(parents=True, exist_ok=True)
    path = child(directory, stem + '.json')
    save_json(path, data)
    return {'path': str(path), 'sha256': digest_file(path)}


def summarize_answers(answers):
    delivery = [a for a in answers if a.get('role') != 'diagnostic']
    diagnostic = [a for a in answers if a.get('role') == 'diagnostic']
    return {'requests': len(answers), 'answer_bytes': sum(a['answer_bytes'] for a in answers),
            'delivery_requests': len(delivery), 'delivery_bytes': sum(a['answer_bytes'] for a in delivery),
            'diagnostic_requests': len(diagnostic), 'diagnostic_bytes': sum(a['answer_bytes'] for a in diagnostic),
            'wire_bytes': sum(a['wire_bytes'] for a in answers),
            'summary': '\n'.join(a['text'] for a in delivery)[:280]}


def evidence_scores(spec, answers, state, rules):
    text = '\n'.join(a['text'] for a in answers)
    results = {}
    for key, facts in substitute(spec['evidence'], state).items():
        checks = []
        text_checks(facts, text, rules, checks)
        results[key] = {'pass': all(c['pass'] for c in checks), 'checks': checks}
    return {'correct': sum(item['pass'] for item in results.values()), 'total': len(results), 'items': results}


def run_question(exe, root, spec, state, config, supports, artifacts):
    result = {'id': spec['id'], 'kind': spec['kind'], 'title': spec['title'], 'fixture': spec.get('fixture'), 'sample': spec.get('sample')}
    protocols = []
    if spec['kind'] == 'budget' and not supports.get(spec['request'].get('transport', 'cli'), False):
        result.update(status='not_applicable', facts={'status':'not_applicable'}, contract={'status':'not_applicable'}, requests=0, answer_bytes=0,
                      reason='context entry is not advertised', delivery_requests=0, delivery_bytes=0)
        return result
    if spec['kind'] == 'task':
        answers = []
        for req in spec['request']['legacy']:
            answer = invoke(exe, root, cli_arguments(req))
            answer['role'] = 'delivery'
            answers.append(answer)
        facts = grade_facts(spec, answers, None, state, config['rules'])
        evidence = evidence_scores(spec, answers, state, config['rules'])
        for key, value in evidence['items'].items():
            if not value['pass']:
                facts['checks'].append({'check':'required task evidence ' + key, 'pass':False})
                facts['status'] = 'fail'
        result.update(summarize_answers(answers))
        result.update(facts=facts, contract={'status':'not_applicable'}, evidence=evidence, status=facts['status'])
        raw = {'answers':answers, 'state':state}
        if supports['cli']:
            req = dict(spec['request']['context'], transport='cli')
            answer, protocol = ask(exe, root, req)
            context_facts = grade_facts(spec, [answer], None, state, config['rules'])
            context_evidence = evidence_scores(spec, [answer], state, config['rules'])
            result['context'] = dict(summarize_answers([answer]), facts=context_facts, evidence=context_evidence,
                                     status=context_facts['status'], budget=req['budget'])
            result['context']['within_budget'] = len(answer['text'].encode('utf-8')) <= req['budget']
            raw['context_answers'] = [answer]
        else:
            result['context'] = {'status':'not_applicable', 'reason':'context command is not advertised'}
        result['raw'] = artifact_bundle(artifacts, spec['id'], raw)
        return result
    answer, protocol = ask(exe, root, spec['request'])
    answer['role'] = 'delivery'
    answers = [answer]
    if protocol:
        protocols.append(protocol)
    report = None
    if spec['kind'] == 'structure':
        diagnostic = invoke(exe, root, cli_arguments(spec['request']) + ['--json'])
        diagnostic['role'] = 'diagnostic'
        answers.append(diagnostic)
        if not diagnostic['error']:
            try:
                report = json.loads(diagnostic['text'])
                if not isinstance(report, dict):
                    report = None
            except json.JSONDecodeError:
                pass
    facts = grade_facts(spec, answers, report, state, config['rules'])
    expected_contract = grade_contract(spec, report, state)
    result.update(summarize_answers(answers))
    result.update(status='pass' if facts['status']=='pass' and expected_contract['status']!='fail' else 'fail',
                  facts=facts, contract=expected_contract, report_contract=(report or {}).get('contract', 1),
                  protocol_requests=sum(p['protocol_requests'] for p in protocols),
                  protocol_response_bytes=sum(p['protocol_response_bytes'] for p in protocols))
    result['raw'] = artifact_bundle(artifacts, spec['id'], {'answers':answers, 'protocols':protocols, 'state':state})
    return result


def read_labels(path):
    with Path(path).open(encoding='utf-8-sig', newline='') as stream:
        reader = csv.DictReader(stream, delimiter='\t')
        if reader.fieldnames != ['id','category','source','line','target','basis']:
            raise ValueError('Invalid label header: ' + str(reader.fieldnames))
        rows = list(reader)
    ids = set()
    pairs = set()
    for row in rows:
        if row['id'] in ids or row['category'] not in ('exact','likely','ambiguous','none'):
            raise ValueError('Duplicate id or invalid category: ' + repr(row))
        ids.add(row['id'])
        row['line'] = int(row['line'])
        if row['line'] < 1 or not row['basis'].strip():
            raise ValueError('Missing label basis/line')
        for key in ('source','target'):
            if '\\' in row[key] or ':' in row[key] or Path(row[key]).is_absolute() or '..' in Path(row[key]).parts:
                raise ValueError('Unsafe label path')
        signature = (row['category'], row['source'], row['line'], row['target'])
        if signature in pairs:
            raise ValueError('Duplicate labeled relation: ' + repr(signature))
        pairs.add(signature)
    positive = sum(r['category'] in ('exact','likely') for r in rows)
    if len(rows) < 30 or positive < 20 or sum(r['category']=='none' for r in rows) < 5:
        raise ValueError('Labels require >=30 rows, >=20 positive and >=5 none')
    return rows


def relation_grade(row, report):
    relations = report.get('relations', [])
    at_line = [r for r in relations if r.get('target') == row['target'] and r.get('line') == row['line']]
    category = row['category']
    if category == 'none':
        return (not at_line), ('false_positive' if at_line else None), bool(at_line)
    if category == 'ambiguous':
        issues = [r for r in report.get('issues', []) if r.get('source') == row['source'] and r.get('line') == row['line']]
        connected = any(r.get('target') == row['target'] for r in relations)
        ambiguous = any(r.get('state') == 'ambiguous' and row['target'] in r.get('candidates', []) for r in issues)
        return ambiguous and not connected, ('connected_ambiguous_target' if connected else 'missing_ambiguous_candidate') if connected or not ambiguous else None, False
    if at_line:
        ok = any(r.get('confidence') == category for r in at_line)
        return ok, None if ok else 'wrong_confidence', True
    if any(r.get('target') == row['target'] for r in relations):
        return False, 'wrong_line', False
    return False, 'missing_relation', False


def reported_precision(labels, reports):
    """Precision of what the tool reported, by the confidence it claimed (UPGRADE section 4).

    Only relations on a labeled (source, line) with the labeled target are judged; a missing
    relation is a recall miss, not a precision error. A reported relation is wrong when it
    connects a `none`/`ambiguous` label, or claims `exact` where the label says `likely`.
    """
    out = {c:{'total':0,'correct':0,'errors':[]} for c in ('exact','likely')}
    for row in labels:
        for relation in reports[row['source']].get('relations', []):
            claimed = relation.get('confidence')
            if (relation.get('line') != row['line'] or relation.get('target') != row['target']
                    or relation.get('source') != row['source'] or claimed not in out):
                continue
            wrong = None
            if row['category'] in ('none','ambiguous'):
                wrong = 'connected_' + row['category']
            elif claimed == 'exact' and row['category'] == 'likely':
                wrong = 'overclaimed'
            out[claimed]['total'] += 1
            if wrong:
                out[claimed]['errors'].append({'id':row['id'],'reason':wrong})
            else:
                out[claimed]['correct'] += 1
    for row in out.values():
        row['precision'] = row['correct']/row['total'] if row['total'] else None
    return out


def run_relations(exe, sample_roots, sample_states, config, labels_root, artifacts):
    results = {}
    for name, meta in config['samples'].items():
        result = {'language':meta['language'], 'sample':name}
        results[name] = result
        if name not in sample_roots:
            result.update(status='error', reason='sample missing or rejected')
            continue
        result['index_report'] = sample_states[name]['index_reports'][-1]
        label_path = Path(labels_root) / (name + '.tsv')
        if not label_path.exists():
            result.update(status='pending_labels', reason='待标签就绪后补跑', labels_path=str(label_path))
            continue
        try:
            labels = read_labels(label_path)
            result['labels_sha256'] = digest_file(label_path)
            result['labels_count'] = len(labels)
            root = sample_roots[name]
            for row in labels:
                for key in ('source','target'):
                    if not child(root, row[key]).is_file():
                        raise ValueError('Label path does not exist in sample: ' + row[key])
            sources = sorted({r['source'] for r in labels})
            reports, raw_answers = {}, []
            for source in sources:
                answer = checked(exe, root, ['explain',source,'--json'])
                report = json.loads(answer['text'])
                if report.get('truncated'):
                    raise ValueError('Label source query truncated: ' + source)
                reports[source] = report
                raw_answers.append(answer)
            categories = {k:{'total':0,'correct':0,'errors':[]} for k in ('exact','likely','ambiguous','none')}
            linked = 0
            entries = []
            for row in labels:
                report = reports[row['source']]
                ok, reason, connected = relation_grade(row, report)
                entry = dict(row, correct=ok, reason=reason, linked=connected)
                entries.append(entry)
                category = categories[row['category']]
                category['total'] += 1
                category['correct'] += int(ok)
                if not ok:
                    failure = dict(entry)
                    failure['observed_relations'] = report.get('relations', [])
                    failure['observed_issues'] = report.get('issues', [])
                    category['errors'].append(failure)
                if row['category'] in ('exact','likely'):
                    linked += int(connected)
            for category in categories.values():
                category['accuracy'] = category['correct']/category['total'] if category['total'] else None
            total = sum(r['category'] in ('exact','likely') for r in labels)
            result.update(status='complete', categories=categories, labels=entries,
                          reported=reported_precision(labels, reports),
                          recall={'linked':linked,'total':total,'rate':linked/total if total else None},
                          requests=len(sources), answer_bytes=sum(a['answer_bytes'] for a in raw_answers),
                          raw=artifact_bundle(artifacts, 'relations-'+name, {'reports':reports,'answers':raw_answers}))
        except Exception as exc:
            result.update(status='error', reason=str(exc))
    return {'status':'complete' if all(r['status']=='complete' for r in results.values()) else 'incomplete', 'samples':results}


def number_summary(values):
    good = [v for v in values if v is not None]
    if not good:
        return {'median':None,'min':None,'max':None,'rounds':len(values),'observations':0}
    return {'median':statistics.median(good), 'min':min(good), 'max':max(good), 'rounds':len(values),'observations':len(good)}


def resource_measure(exe, root, args):
    result = checked(exe, root, args)
    return {'ms':result['ms'],'peak_working_set_bytes':result['peak_working_set_bytes'],
            'memory_error':result.get('memory_error'),'answer_bytes':result['answer_bytes'],
            'report':json.loads(result['text'])}


def run_resources(builds, pristine, config, root, rounds):
    root = guard(root)
    root.mkdir(parents=True, exist_ok=False)
    pair_id = uuid.uuid4().hex if len(builds) > 1 else None
    samples = {name:{label:[] for label in builds} for name in config['samples']}
    order_log = []
    errors = []
    for name, meta in config['samples'].items():
        if name not in pristine:
            errors.append({'sample':name,'reason':'sample missing or rejected'})
            continue
        labels = list(builds)
        for n in range(rounds):
            order = labels[n % len(labels):] + labels[:n % len(labels)]
            order_log.append({'sample':name,'round':n+1,'order':order})
            for label in order:
                exe = builds[label]
                dest = root/name/label/('round-%02d'%(n+1))
                try:
                    copy_sample(pristine[name], dest)
                    checked(exe, dest, ['init'])
                    measurements = {}
                    measurements['full'] = resource_measure(exe,dest,['index','--json'])
                    measurements['unchanged'] = resource_measure(exe,dest,['index','--json'])
                    path = child(dest,meta['touch'])
                    original = path.read_bytes()
                    try:
                        path.write_bytes(original + b'\n' + meta['comment'].encode('utf-8'))
                        measurements['one_file'] = resource_measure(exe,dest,['index','--json'])
                        for command in ('explain','impact'):
                            measurements[command] = resource_measure(exe,dest,[command,meta['resource_target'],'--json'])
                    finally:
                        path.write_bytes(original)
                    if measurements['unchanged']['report'].get('parsed') != 0:
                        raise ValueError('Unchanged refresh parsed files')
                    if measurements['one_file']['report'].get('parsed') != 1:
                        raise ValueError('Single-file refresh did not parse exactly one file')
                    samples[name][label].append({'round':n+1, 'measurements':measurements})
                except Exception as exc:
                    errors.append({'sample':name,'build':label,'round':n+1,'reason':str(exc)})
    idle = {label:[] for label in builds}
    # A single serve session per required sample/build; fixed calls, then 30s idle.
    for name, meta in config['samples'].items():
        if name not in pristine:
            continue
        for label, exe in builds.items():
            dest = root/name/label/'idle'
            client = None
            try:
                copy_sample(pristine[name], dest)
                checked(exe,dest,['init'])
                checked(exe,dest,['index','--json'])
                client = MCP(exe,dest)
                calls = [client.call('overview',{}),
                         client.call('explain',{'target':meta['resource_target']}),
                         client.call('impact',{'target':meta['resource_target']}),
                         client.call('recall',{'query':'fixed-eval-no-match','top_k':5})]
                if any(c['error'] for c in calls):
                    raise RuntimeError('Fixed MCP resource call failed')
                before = memory(client.proc)
                start = time.perf_counter()
                time.sleep(30)
                observed = memory(client.proc)
                observed.update(sample=name,idle_seconds=time.perf_counter()-start, before_idle=before,
                                knowledge_requests=len(calls), answers=calls)
                observed['session'] = client.close()
                client = None
                if observed['session']['returncode'] != 0:
                    raise RuntimeError('serve failed to exit cleanly')
                idle[label].append(observed)
            except Exception as exc:
                errors.append({'sample':name,'build':label,'stage':'idle','reason':str(exc)})
            finally:
                if client:
                    client.close()
    by_build = {}
    for label in builds:
        summary = {}
        for name in config['samples']:
            runs = samples[name][label]
            operations = {}
            for operation in ('full','unchanged','one_file','explain','impact'):
                operations[operation] = {'ms':number_summary([r['measurements'][operation]['ms'] for r in runs]),
                                         'peak_working_set_bytes':number_summary([r['measurements'][operation]['peak_working_set_bytes'] for r in runs])}
            summary[name] = {'operations':operations,'runs':runs,
                             'fixed_target':config['samples'][name]['resource_target'],
                             'target_relation_counts':{op:(runs[0]['measurements'][op]['report'].get('relations') if op in ('full','unchanged','one_file') else len(runs[0]['measurements'][op]['report'].get('relations',[]))) for op in ('full','explain','impact')} if runs else {}}
        by_build[label] = {'status':'error' if errors else 'complete','trial_only':len(builds)==1,
                           'note':'单版本试跑；正式对照请 --peer-loom 同场轮换' if len(builds)==1 else '同一调度、逐轮轮换版本先后；文件缓存不清空',
                           'pair_id':pair_id,'interleaved':len(builds)>1,'rounds':rounds,'order':order_log,
                           'samples':summary,'idle':idle[label],'errors':errors}
    return by_build


def validate_samples(config, mapping):
    results, sources = {}, {}
    for name, meta in config['samples'].items():
        source = Path(mapping.get(name, EVAL_HOME / meta['source'])).expanduser().resolve()
        record = {'source':str(source),'expected_revision':meta['revision'],'language':meta['language']}
        results[name] = record
        try:
            if not source.is_dir():
                raise ValueError('Sample directory does not exist')
            if source == HERE.parent.resolve() or (source / 'eval' / 'run.py').exists():
                raise ValueError('Refuse development repository as a sample; use the pinned export')
            if meta.get('head'):
                head = read_git_head(source)
                record['head'] = head
                if head != meta['head']:
                    raise ValueError('HEAD mismatch: ' + head)
            elif (source/'.git').exists():
                raise ValueError('Pinned no-git export unexpectedly has .git')
            exclude = source / '.git' / 'info' / 'exclude'
            exclude_digest = digest_file(exclude) if exclude.is_file() else None
            record['git_info_exclude_sha256'] = exclude_digest
            if exclude_digest != meta.get('git_info_exclude_sha256'):
                raise ValueError('Git info/exclude scope input mismatch')
            actual = source_inventory(source)
            record['manifest'] = actual
            if any(actual[k] != meta['manifest'][k] for k in ('sha256','files')):
                raise ValueError('Source manifest mismatch')
            record['status'] = 'verified'
            sources[name] = source
        except Exception as exc:
            record.update(status='rejected',reason=str(exc))
    return results, sources


def discover(exe, root):
    root = guard(root)
    root.mkdir(parents=True,exist_ok=True)
    help_result = checked(exe,root,['--help'])
    version = checked(exe,root,['--version'])
    supports_cli = bool(re.search(r'^\s+context\s',help_result['text'],re.M))
    # Inspect actual MCP advertisement rather than assuming CLI/MCP feature parity.
    checked(exe,root,['init'])
    client = MCP(exe,root)
    try:
        reply,_ = client.send('tools/list')
        tools = reply.get('result',{}).get('tools',[])
        supports_mcp = any(t.get('name')=='context' for t in tools)
    finally:
        session = client.close()
    return {'cli':supports_cli,'mcp':supports_mcp}, {'help':help_result,'version':version,'tools':tools,'session':session}


def parse_samples(value, config):
    if not value:
        return {}
    if value.lstrip().startswith('{'):
        return json.loads(value)
    path = Path(value)
    if path.is_file():
        data = load_json(path)
        return data.get('samples',data)
    if path.is_dir():
        return {name:str(path/name) for name in config['samples']}
    raise ValueError('--samples must be a mapping JSON, a mapping file or directory')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--loom', required=True)
    parser.add_argument('--samples', help='JSON name-to-source map, its file, or parent with named sample directories; defaults are LOOM_EVAL_HOME/samples/<name>; revisions and manifests stay pinned in questions.json')
    parser.add_argument('--work', required=True, help='Fresh strict child of LOOM_EVAL_HOME/work; LOOM_EVAL_HOME defaults to ~/.cache/loom-eval and must end in loom-eval')
    parser.add_argument('--out', required=True)
    parser.add_argument('--only', choices=('relations','questions','resources'))
    parser.add_argument('--rounds',type=int,default=5)
    parser.add_argument('--peer-loom',help='Run resources against this executable, interleaved by round')
    parser.add_argument('--peer-out',help='Write paired peer resource result JSON (required with --peer-loom)')
    parser.add_argument('--labels',default=str(HERE/'labels'))
    parser.add_argument('--merge-from',help='With --only, preserve other sections from a result with the identical executable and frozen inputs; writes a NEW result')
    parser.add_argument('--relation-self-test',action='store_true',help='Exercise all label grader paths with a synthetic fixture; no real labels needed')
    args = parser.parse_args()
    if hasattr(sys.stdout,'reconfigure'):
        sys.stdout.reconfigure(encoding='utf-8')
    if not sys.flags.isolated:
        parser.error('Use python -I eval/run.py')
    try:
        work = guard(args.work)
        if work.exists():
            raise ValueError('Use a fresh --work directory; existing directories are never reused')
        if args.rounds < 5:
            raise ValueError('--rounds must be at least 5, including trial runs')
        if bool(args.peer_loom) != bool(args.peer_out):
            raise ValueError('--peer-loom and --peer-out must be supplied together')
        if args.merge_from and (not args.only or args.peer_loom):
            raise ValueError('--merge-from requires --only and cannot be combined with --peer-loom')
        exe = refuse_production(args.loom)
        peer = refuse_production(args.peer_loom) if args.peer_loom else None
        output = Path(args.out).resolve()
        if output.exists():
            raise ValueError('Refuse overwriting existing result; choose a fresh --out')
        if args.peer_out and Path(args.peer_out).resolve().exists():
            raise ValueError('Refuse overwriting peer result')
        config = load_json(HERE/'questions.json')
        thresholds = load_json(HERE/'thresholds.json')
        if config['allowed_work_root'] != 'work':
            raise ValueError('Configured work root must be the dedicated work directory')
        work.mkdir(parents=True)
        artifacts = work/'raw'
        artifacts.mkdir()
        supports, discovery = discover(exe,work/'discovery')
        result = {'schema':1,'suite':config['suite'],'run_id':uuid.uuid4().hex,
                  'started_at':datetime.now(timezone.utc).isoformat(),'work':str(work),
                  'executable':{'path':str(exe),'sha256':digest_file(exe),'version':discovery['version']['text'].strip(),
                                'matches_rev8_baseline':digest_file(exe)==config['baseline']['sha256']},
                  'definitions':{'questions_sha256':digest_file(HERE/'questions.json'),'thresholds_sha256':digest_file(HERE/'thresholds.json')},
                  'harness':{'run_sha256':digest_file(__file__),'compare_sha256':digest_file(HERE/'compare.py')},
                  'invocation':sys.argv[1:],
                  'environment':{'python':sys.version,'isolated':bool(sys.flags.isolated),'platform':platform.platform(),
                                 'logical_cpus':os.cpu_count(),'network':'none; no sample execution; LOOM_* unset',
                                 'memory_method':'psapi GetProcessMemoryInfo, 5ms polling plus post-exit handle peak',
                                 'bytes':'UTF-8 CLI stdout+stderr or MCP content text; wire separately; no token estimates'},
                  'features':{'context':supports},'discovery':artifact_bundle(artifacts,'discovery',discovery),
                  'sections':{'relations':{'status':'not_run'},'questions':{'status':'not_run'},'resources':{'status':'not_run'}}}
        mapping = parse_samples(args.samples,config)
        sample_validation, sources = validate_samples(config,mapping)
        result['samples'] = sample_validation
        pristine, sample_roots, sample_states = {}, {}, {}
        for name, source in sources.items():
            try:
                copied = copy_sample(source,work/'pristine'/name)
                copied_manifest = source_inventory(copied)
                if any(copied_manifest[k] != config['samples'][name]['manifest'][k] for k in ('sha256','files')):
                    raise ValueError('Source changed while making sample copy')
                pristine[name] = copied
                if args.only != 'resources':
                    dest = copy_sample(pristine[name],work/'samples'/name)
                    init = checked(exe,dest,['init'])
                    indexed = checked(exe,dest,['index','--json'])
                    sample_roots[name] = dest
                    sample_states[name] = {'project_id':identifier(init['text'],'project id'),
                                           'index_reports':[json.loads(indexed['text'])]}
            except Exception as exc:
                result['samples'][name].update(status='rejected',reason=str(exc))
        if args.relation_self_test:
            result['relation_self_test'] = relation_self_test(exe,work,artifacts)
        if args.only in (None,'relations'):
            print('Running labeled relations',flush=True)
            result['sections']['relations'] = run_relations(exe,sample_roots,sample_states,config,args.labels,artifacts)
        if args.only in (None,'questions'):
            rows = []
            for spec in config['questions']:
                print('Question '+spec['id']+' '+spec['title'],flush=True)
                try:
                    if spec.get('sample'):
                        root = sample_roots[spec['sample']]
                        state = sample_states[spec['sample']]
                    else:
                        root = work/'questions'/spec['id']
                        state = prepare(exe,root,config['fixtures'][spec['fixture']])
                    row = run_question(exe,root,spec,state,config,supports,artifacts)
                    rows.append(row)
                except Exception as exc:
                    rows.append({'id':spec['id'],'kind':spec['kind'],'title':spec['title'],'status':'error',
                                 'facts':{'status':'error'},'contract':{'status':'not_applicable'},'reason':str(exc),
                                 'requests':None,'answer_bytes':None})
            result['sections']['questions'] = {'status':'complete' if all(r['status']!='error' for r in rows) else 'error',
                                              'results':rows,'false_negatives':sum(len(r.get('facts',{}).get('false_negatives',[])) + len(r.get('context',{}).get('facts',{}).get('false_negatives',[])) for r in rows)}
        if args.only in (None,'resources'):
            print('Resources: >=5 rounds; each sample/build then idles serve for 30 seconds',flush=True)
            builds = {'primary':exe}
            if peer:
                builds['peer'] = peer
            resources = run_resources(builds,pristine,config,work/'resources',args.rounds)
            result['sections']['resources'] = resources['primary']
            if peer:
                peer_result = copy.deepcopy(result)
                peer_result['run_id'] = uuid.uuid4().hex
                peer_result['executable'] = {'path':str(peer),'sha256':digest_file(peer),'version':None,
                                             'matches_rev8_baseline':digest_file(peer)==config['baseline']['sha256']}
                peer_result['sections'] = {'relations':{'status':'not_run'},'questions':{'status':'not_run'},'resources':resources['peer']}
                peer_result['note'] = 'Peer resource-only report; run quality suite separately.'
                save_json(args.peer_out,peer_result)
        result['finished_at'] = datetime.now(timezone.utc).isoformat()
        if args.merge_from:
            previous = load_json(args.merge_from)
            same = (previous.get('suite')==result['suite'] and previous.get('definitions')==result['definitions']
                    and previous.get('executable',{}).get('sha256')==result['executable']['sha256']
                    and previous.get('harness',{}).get('run_sha256')==result['harness']['run_sha256']
                    and all(previous.get('samples',{}).get(name,{}).get('manifest')==data.get('manifest')
                            for name,data in result['samples'].items()))
            if not same:
                raise ValueError('--merge-from binary, runner, definitions or sample manifests differ')
            for section,value in result['sections'].items():
                if value['status']=='not_run':
                    result['sections'][section]=previous['sections'][section]
            result['merged_from']={'path':str(Path(args.merge_from).resolve()),'sha256':digest_file(args.merge_from),
                                   'run_id':previous['run_id'],'replaced_section':args.only,
                                   'previous_harness':previous.get('harness'),'previous_work':previous.get('work')}
        result['execution_ok'] = (all(s['status']=='verified' for s in result['samples'].values())
                                  and all(s['status']!='error' for s in result['sections'].values())
                                  and all(s['status']!='error' for s in result['sections']['relations'].get('samples',{}).values()))
        save_json(output,result)
        print('Saved '+str(output),flush=True)
        print('Execution OK='+str(result['execution_ok'])+'; factual failures are retained, not infrastructure failures.',flush=True)
        return 0 if result['execution_ok'] else 2
    except Exception as exc:
        print('ERROR: '+str(exc),file=sys.stderr)
        return 2


def relation_self_test(exe, work, artifacts):
    """Real CLI reports, synthetic labels; never treated as real-sample quality."""
    root = work/'relation-self-test'
    fixture = {'files':{'entry.ts':"import './a.ts';\nimport './b.js';\nimport './c.js';\n",'a.ts':'export const a=1;\n','b.ts':'export const b=1;\n','c.ts':'export const c=1;\n','c.js':'export const c=2;\n','unrelated.ts':'export const x=1;\n'}}
    prepare(exe,root,fixture)
    answer = checked(exe,root,['explain','entry.ts','--json'])
    report = json.loads(answer['text'])
    cases = [('exact','a.ts',1,True,None),('likely','b.ts',2,True,None),('ambiguous','c.ts',3,True,None),
             ('none','unrelated.ts',1,True,None),('likely','a.ts',1,False,'wrong_confidence'),
             ('exact','a.ts',2,False,'wrong_line'),('exact','unrelated.ts',1,False,'missing_relation'),
             ('none','a.ts',1,False,'false_positive'),('ambiguous','a.ts',1,False,'connected_ambiguous_target')]
    checks = []
    for category,target,line,expected,reason in cases:
        row = {'category':category,'source':'entry.ts','target':target,'line':line}
        actual,why,_ = relation_grade(row,report)
        checks.append({'label':row,'pass':actual==expected and why==reason,'actual':actual,'reason':why})
    if not all(c['pass'] for c in checks):
        raise RuntimeError('Synthetic relation grader self-test failed: '+repr(checks))
    return {'status':'pass','checks':checks,'raw':artifact_bundle(artifacts,'relation-self-test',{'answer':answer,'report':report})}


if __name__ == '__main__':
    raise SystemExit(main())
