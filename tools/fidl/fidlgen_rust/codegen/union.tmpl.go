// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package codegen

const unionTmpl = `
{{- define "UnionDeclaration" }}
{{- range .DocComments}}
///{{ . }}
{{- end}}
{{ .Derives }}
pub enum {{ .Name }} {
	{{- range .Members }}
	{{- range .DocComments }}
	///{{ . }}
	{{- end }}
	{{ .Name }}({{ .Type }}),
	{{- end }}
	{{- if .IsFlexible }}
	#[deprecated = "Use ` + "`{{ .Name }}::unknown()` to construct and `{{ .Name }}Unknown!()`" + ` to exhaustively match."]
	#[doc(hidden)]
	__Unknown {
		ordinal: u64,
		{{- if .IsResourceType }}
		data: fidl::UnknownData,
		{{- else }}
		bytes: Vec<u8>,
		{{- end }}
	},
	{{- end }}
}

{{- if .IsFlexible }}
/// Pattern that matches an unknown {{ .Name }} member.
#[macro_export]
macro_rules! {{ .Name }}Unknown {
	() => { _ };
}
{{- end }}

impl {{ .Name }} {
{{- if and .IsStrict .IsValueType }}
	#[deprecated = "Strict unions should not use validate()"]
	#[inline]
	pub fn validate(self) -> std::result::Result<Self, (u64, Vec<u8>)> {
		Ok(self)
	}

	#[deprecated = "Strict unions should not use is_unknown()"]
	#[inline]
	pub fn is_unknown(&self) -> bool {
		false
	}
{{- end }}

{{- if and .IsStrict .IsResourceType }}
	#[deprecated = "Strict unions should not use validate()"]
	#[inline]
	pub fn validate(self) -> std::result::Result<Self, (u64, fidl::UnknownData)> {
		Ok(self)
	}

	#[deprecated = "Strict unions should not use is_unknown()"]
	#[inline]
	pub fn is_unknown(&self) -> bool {
		false
	}
{{- end }}

{{- if and .IsFlexible .IsValueType }}
	#[inline]
	pub fn unknown(ordinal: u64, bytes: Vec<u8>) -> Self {
		#[allow(deprecated)]
		Self::__Unknown { ordinal, bytes }
	}

	#[inline]
	pub fn validate(self) -> std::result::Result<Self, (u64, Vec<u8>)> {
		match self {
			#[allow(deprecated)]
			Self::__Unknown { ordinal, bytes } => Err((ordinal, bytes)),
			_ => Ok(self)
		}
	}

	#[inline]
	pub fn is_unknown(&self) -> bool {
		match self {
			#[allow(deprecated)]
			Self::__Unknown { .. } => true,
			_ => false,
		}
	}
{{- end }}

{{- if and .IsFlexible .IsResourceType }}
	#[inline]
	pub fn unknown(ordinal: u64, data: fidl::UnknownData) -> Self {
		#[allow(deprecated)]
		Self::__Unknown { ordinal, data }
	}

	#[inline]
	pub fn validate(self) -> std::result::Result<Self, (u64, fidl::UnknownData)> {
		match self {
			#[allow(deprecated)]
			Self::__Unknown { ordinal, data } => Err((ordinal, data)),
			_ => Ok(self)
		}
	}

	#[inline]
	pub fn is_unknown(&self) -> bool {
		match self {
			#[allow(deprecated)]
			Self::__Unknown { .. } => true,
			_ => false,
		}
	}
{{- end }}
}

fidl_union! {
	name: {{ .Name }},
	members: [
	{{- range .Members }}
		{{ .Name }} {
			ty: {{ .Type }},
			ordinal: {{ .Ordinal }},
			{{- if .HasHandleMetadata }}
			handle_metadata: {
				handle_subtype: {{ .HandleSubtype }},
				handle_rights: {{ .HandleRights }},
			},
			{{- end }}
		},
	{{- end }}
	],
	{{- if .IsFlexible }}
	{{ if .IsResourceType }}resource{{ else }}value{{ end }}_unknown_member: __Unknown,
	{{- end }}
}
{{ end }}
`
