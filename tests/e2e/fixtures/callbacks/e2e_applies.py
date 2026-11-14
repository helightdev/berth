"""An Ansible callback that notes each playbook run of the end-to-end tests
in the file BERTH_E2E_APPLIES names: when it ended, how long it took and
what its hosts did and which tasks took longest. harness.py counts them
for the summary of a run."""
import json
import os
import time

from ansible.plugins.callback import CallbackBase


class CallbackModule(CallbackBase):
    CALLBACK_VERSION = 2.0
    CALLBACK_TYPE = 'aggregate'
    CALLBACK_NAME = 'e2e_applies'
    CALLBACK_NEEDS_ENABLED = True

    def __init__(self):
        super().__init__()
        self.started = time.time()
        self.playbook = None
        self.tasks = {}
        self.task = None

    def v2_playbook_on_start(self, playbook):
        self.started = time.time()
        self.playbook = getattr(playbook, '_file_name', None)

    def _task_ended(self):
        if self.task:
            name, began = self.task
            self.tasks[name] = self.tasks.get(name, 0) + time.time() - began
            self.task = None

    def v2_playbook_on_task_start(self, task, is_conditional):
        self._task_ended()
        self.task = (task.get_name(), time.time())

    def v2_playbook_on_handler_task_start(self, task):
        self.v2_playbook_on_task_start(task, False)

    def v2_playbook_on_stats(self, stats):
        self._task_ended()
        path = os.environ.get('BERTH_E2E_APPLIES')
        if not path:
            return
        hosts = {host: stats.summarize(host) for host in sorted(stats.processed)}
        line = {'ended': time.time(), 'seconds': round(time.time() - self.started, 1),
                'playbook': self.playbook, 'hosts': hosts,
                'slowest': sorted(((round(seconds, 1), name) for name, seconds in self.tasks.items()),
                                  reverse=True)[:8]}
        with open(path, 'a', encoding='utf-8') as file:
            file.write(json.dumps(line) + '\n')
