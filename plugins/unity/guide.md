# Unity

The Unity CLI is `unity` (on the PATH that `agent-env` and login shells provide).
Its skills are registered with the coding agents installed here; prefer them and
`unity --help` over guessing commands.

## Before the first use

1. Signing in needs a person once: `unity auth login` prints a URL to open.
   `unity auth status` tells you whether that has happened. Do not work around a
   missing sign-in.
2. The license is yours to handle. When `unity license status` shows none active,
   or the editor reports "No valid Unity Editor license found":

       unity license activate
       unity license activate --personal --accept-eula   # if the first activated nothing

   The owner has agreed to those terms for this machine; do not ask again.

## Editors

{% if 'unity/editor' in devbox_plugin_features %}
{{ devbox_unity_editor_version | default('6000.7.0b8') }} is preinstalled. Install another when a project asks for it.
{% else %}
None is preinstalled; they are large and belong to a project.
{% endif %}

    unity editors --installed
    unity install VERSION --module webgl --yes --accept-eula
    unity modules VERSION            # what an installed editor can add

Editors land in `~/Unity/Hub/Editor/VERSION`. Pick the version the project asks
for (`unity projects info PATH`).

## Running

- For builds, tests and scripted work use `unity build`, `unity test` and
  `unity run`; they run the editor in batch mode and need no window.
- To drive a live editor, start it on the shared desktop with `agent-env unity
  open PATH` and read `/etc/devbox/guides/desktop.md` first.
{% if devbox_software_rendering %}
- This machine renders on the CPU. Pass `-force-glcore` where software Vulkan is
  not supported, and expect the editor window to be slow.
{% endif %}
- Set `UNITY_NON_INTERACTIVE=1` in scripts so nothing waits for an answer.
