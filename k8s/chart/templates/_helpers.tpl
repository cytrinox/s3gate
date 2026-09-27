{{/* Chart name, truncated to the 63-character DNS label limit. */}}
{{- define "s3gate.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" }}
{{- end }}

{{/* Fully qualified app name. */}}
{{- define "s3gate.fullname" -}}
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

{{- define "s3gate.chart" -}}
{{- printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" | trunc 63 | trimSuffix "-" }}
{{- end }}

{{- define "s3gate.labels" -}}
helm.sh/chart: {{ include "s3gate.chart" . }}
{{ include "s3gate.selectorLabels" . }}
app.kubernetes.io/version: {{ .Values.image.tag | default .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end }}

{{- define "s3gate.selectorLabels" -}}
app.kubernetes.io/name: {{ include "s3gate.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end }}

{{- define "s3gate.serviceAccountName" -}}
{{- if .Values.serviceAccount.create }}
{{- default (include "s3gate.fullname" .) .Values.serviceAccount.name }}
{{- else }}
{{- default "default" .Values.serviceAccount.name }}
{{- end }}
{{- end }}

{{/* Secret holding the S3 credentials: the existing one, or the one this chart creates. */}}
{{- define "s3gate.secretName" -}}
{{- .Values.credentials.existingSecret | default (include "s3gate.fullname" .) }}
{{- end }}

{{- define "s3gate.image" -}}
{{- if .Values.image.digest }}
{{- printf "%s@%s" .Values.image.repository .Values.image.digest }}
{{- else }}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) }}
{{- end }}
{{- end }}

{{/* Fails rendering on configuration s3gate would reject at startup. */}}
{{- define "s3gate.validate" -}}
{{- if not .Values.s3.bucket }}
{{- fail "s3.bucket is required" }}
{{- end }}
{{- if and (not .Values.credentials.existingSecret) (not (and .Values.credentials.accessKeyId .Values.credentials.secretAccessKey)) }}
{{- fail "set credentials.existingSecret, or both credentials.accessKeyId and credentials.secretAccessKey" }}
{{- end }}
{{- if not (has .Values.config.logFormat (list "json" "text")) }}
{{- fail "config.logFormat must be json or text" }}
{{- end }}
{{- end }}
