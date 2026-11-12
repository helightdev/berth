FROM ubuntu:26.04
ARG AGENT_USER=agent
ARG OPERATOR_USER=ops
ARG INSTALL_PLAYWRIGHT=true
ARG INSTALL_WEB_TOOLS=true
ARG INSTALL_NATIVE_TOOLS=true
ARG INSTALL_JVM_TOOLS=true
ARG INSTALL_DOTNET_TOOLS=true
ARG INSTALL_SCRIPTING_TOOLS=true
ARG INSTALL_SCIENTIFIC_TOOLS=true
ARG INSTALL_CHROME_TOOLS=true
# JSON list of plugin features from plugins/. The coding agents, T3 Code and
# GitHub's CLI are among them; add for example "unity/cli".
ARG PLUGIN_FEATURES='["claude/cli","codex/cli","pi/cli","t3/server","github/cli"]'
ARG SOFTWARE_RENDERING=true
ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-venv ca-certificates sudo dbus && rm -rf /var/lib/apt/lists/*
COPY provision/requirements.txt /tmp/devbox-requirements.txt
RUN python3 -m venv /opt/ansible && /opt/ansible/bin/pip install -r /tmp/devbox-requirements.txt
# Laid out as the checkout is: the role finds the plugins at ../../plugins.
COPY provision/ansible /opt/devbox/provision/ansible
COPY provision/ansible.cfg /opt/devbox/provision/ansible.cfg
COPY plugins /opt/devbox/plugins
WORKDIR /opt/devbox/provision
RUN /opt/ansible/bin/ansible-playbook -i localhost, -c local ansible/site.yml \
    -e "devbox_user=${AGENT_USER}" -e "devbox_operator_user=${OPERATOR_USER}" \
    -e "{\"devbox_profile\":\"container\",\"devbox_install_playwright\":${INSTALL_PLAYWRIGHT},\"devbox_tools_web\":${INSTALL_WEB_TOOLS},\"devbox_tools_native\":${INSTALL_NATIVE_TOOLS},\"devbox_tools_jvm\":${INSTALL_JVM_TOOLS},\"devbox_tools_dotnet\":${INSTALL_DOTNET_TOOLS},\"devbox_tools_scripting\":${INSTALL_SCRIPTING_TOOLS},\"devbox_tools_scientific\":${INSTALL_SCIENTIFIC_TOOLS},\"devbox_tools_chrome\":${INSTALL_CHROME_TOOLS},\"devbox_plugin_features\":${PLUGIN_FEATURES},\"devbox_software_rendering\":${SOFTWARE_RENDERING}}" \
    && rm -rf /var/lib/apt/lists/*
COPY container/entrypoint.sh /usr/local/bin/devbox-container-session
RUN chmod 755 /usr/local/bin/devbox-container-session
USER ${AGENT_USER}
WORKDIR /srv/agent/workspaces
EXPOSE 6080 3773
ENTRYPOINT ["/usr/local/bin/agent-env", "/usr/local/bin/devbox-container-session"]
