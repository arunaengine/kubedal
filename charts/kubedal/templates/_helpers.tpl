{{- define "kubedal.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "kubedal.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "kubedal.labels" -}}
app.kubernetes.io/name: {{ include "kubedal.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{- end -}}

{{- define "kubedal.image" -}}
{{- printf "%s:%s" .Values.image.repository (default .Chart.AppVersion .Values.image.tag) -}}
{{- end -}}

{{- define "kubedal.controllerServiceAccount" -}}
{{- default (printf "%s-controller" (include "kubedal.fullname" .)) .Values.serviceAccount.controller.name -}}
{{- end -}}

{{- define "kubedal.nodeServiceAccount" -}}
{{- default (printf "%s-node" (include "kubedal.fullname" .)) .Values.serviceAccount.node.name -}}
{{- end -}}

{{- define "kubedal.pluginDir" -}}
{{- printf "%s/plugins/%s" (trimSuffix "/" .Values.kubeletDir) .Values.driverName -}}
{{- end -}}

{{- define "kubedal.dataDir" -}}
{{- default (include "kubedal.pluginDir" .) .Values.node.dataDir -}}
{{- end -}}
