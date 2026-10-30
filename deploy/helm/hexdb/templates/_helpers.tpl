{{- define "hexdb.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "hexdb.headless" -}}
{{- printf "%s-hexes" (include "hexdb.fullname" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "hexdb.secretName" -}}
{{- default (include "hexdb.fullname" .) .Values.existingSecret -}}
{{- end -}}

{{- define "hexdb.labels" -}}
app.kubernetes.io/name: hexdb
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "hexdb.selector" -}}
app.kubernetes.io/name: hexdb
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{/* Every hex's discovery address, for HEXDB_NETWORK__PEERS. */}}
{{- define "hexdb.peers" -}}
{{- $name := include "hexdb.fullname" . -}}
{{- $headless := include "hexdb.headless" . -}}
{{- $ns := .Release.Namespace -}}
{{- $peers := list -}}
{{- range $i := until (int .Values.replicas) -}}
{{- $peers = append $peers (printf "%s-%d.%s.%s.svc.cluster.local:7702" $name $i $headless $ns) -}}
{{- end -}}
{{- join "," $peers -}}
{{- end -}}

{{/* storage.disk_mb: diskMb, or 90% of the volume (Gi or Mi), leaving room for compaction. */}}
{{- define "hexdb.diskMb" -}}
{{- if .Values.diskMb -}}
{{- .Values.diskMb -}}
{{- else -}}
{{- $size := toString .Values.persistence.size -}}
{{- if hasSuffix "Gi" $size -}}
{{- div (mul (trimSuffix "Gi" $size | int) 1024 9) 10 -}}
{{- else if hasSuffix "Mi" $size -}}
{{- div (mul (trimSuffix "Mi" $size | int) 9) 10 -}}
{{- else -}}
16384
{{- end -}}
{{- end -}}
{{- end -}}
