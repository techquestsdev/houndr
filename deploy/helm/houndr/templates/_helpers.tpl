{{/*
Expand the name of the chart.
*/}}
{{- define "houndr.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Create a default fully qualified app name.
*/}}
{{- define "houndr.fullname" -}}
{{- if .Values.fullnameOverride }}
{{- .Values.fullnameOverride | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- $name := default .Chart.Name .Values.nameOverride }}
{{- if contains $name .Release.Name }}
{{- .Release.Name | trunc 63 | trimSuffix "-" }}
{{- else }}
{{- printf "%s-%s" .Release.Name $name | trunc 63 | trimSuffix "-" }}
{{- end }}
{{- end }}
{{- end }}

{{/*
Create chart name and version as used by the chart label.
*/}}
{{- define "houndr.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/*
Common labels
*/}}
{{- define "houndr.labels" -}}
helm.sh/chart: {{ include "houndr.chart" . }}
{{ include "houndr.selectorLabels" . }}
{{- if .Chart.AppVersion }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
{{- end }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{/*
Selector labels
*/}}
{{- define "houndr.selectorLabels" -}}
app.kubernetes.io/name: {{ include "houndr.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{/*
Create the name of the service account to use
*/}}
{{- define "houndr.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "houndr.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/*
Image helper
*/}}
{{- define "houndr.image" -}}
{{- $registry := .imageRegistry | default "" -}}
{{- $repository := .image.repository -}}
{{- $tag := .image.tag | default .appVersion -}}
{{- if $registry }}
{{- printf "%s/%s:%s" $registry $repository $tag }}
{{- else }}
{{- printf "%s:%s" $repository $tag }}
{{- end }}
{{- end }}

{{/*
Render structured config values to TOML
*/}}
{{- define "houndr.config" -}}
[server]
bind = {{ .Values.config.server.bind | quote }}
timeout_secs = {{ .Values.config.server.timeout_secs | int }}
{{- with .Values.config.server.cors_origins }}
cors_origins = [{{ range $i, $v := . }}{{ if $i }}, {{ end }}{{ $v | quote }}{{ end }}]
{{- end }}
{{- with .Values.config.server.rate_limit_rps }}
rate_limit_rps = {{ . | int }}
{{- end }}
{{- with .Values.config.server.max_request_bytes }}
max_request_bytes = {{ . | int }}
{{- end }}
{{- with .Values.config.server.max_search_results }}
max_search_results = {{ . | int }}
{{- end }}

[indexer]
data_dir = {{ .Values.config.indexer.data_dir | quote }}
max_concurrent_indexers = {{ .Values.config.indexer.max_concurrent_indexers | int }}
poll_interval_secs = {{ .Values.config.indexer.poll_interval_secs | int }}
max_file_size = {{ .Values.config.indexer.max_file_size | int }}
{{- with .Values.config.indexer.exclude_patterns }}
exclude_patterns = [{{ range $i, $v := . }}{{ if $i }}, {{ end }}{{ $v | quote }}{{ end }}]
{{- end }}
index_timeout_secs = {{ .Values.config.indexer.index_timeout_secs | int }}

[cache]
max_entries = {{ .Values.config.cache.max_entries | int }}
ttl_secs = {{ .Values.config.cache.ttl_secs | int }}
{{- range .Values.config.repos }}

[[repos]]
name = {{ .name | quote }}
url = {{ .url | quote }}
{{- with .ref }}
ref = {{ . | quote }}
{{- end }}
{{- with .auth_token }}
auth_token = {{ . | quote }}
{{- end }}
{{- with .ssh_key }}
ssh_key = {{ . | quote }}
{{- end }}
{{- with .ssh_key_path }}
ssh_key_path = {{ . | quote }}
{{- end }}
{{- with .ssh_key_passphrase }}
ssh_key_passphrase = {{ . | quote }}
{{- end }}
{{- end }}
{{- end -}}
