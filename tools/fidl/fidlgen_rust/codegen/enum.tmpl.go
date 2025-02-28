// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package codegen

const enumTmpl = `
{{- define "EnumDeclaration" -}}
{{- range .DocComments}}
///{{ . }}
{{- end}}
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
{{- if .IsStrict }}
#[repr({{ .Type }})]
{{- else }}
#[non_exhaustive]
{{- end }}
pub enum {{ .Name }} {
	{{- range .Members }}
	{{- range .DocComments }}
	///{{ . }}
	{{- end }}
	{{ .Name }}{{ if $.IsStrict }} = {{ .Value }}{{ end }},
	{{- end }}
	{{- if .IsFlexible }}
	#[deprecated = "Use ` + "`{{ .Name }}::unknown()` to construct and `{{ .Name }}Unknown!()`" + ` to exhaustively match."]
	#[doc(hidden)]
	__Unknown({{ .Type }}),
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
	#[inline]
	pub fn from_primitive(prim: {{ .Type }}) -> Option<Self> {
		match prim {
			{{- range .Members }}
			{{ .Value }} => Some(Self::{{ .Name }}),
			{{- end }}
			_ => None,
		}
	}

{{ if .IsStrict }}
	#[inline]
	pub fn into_primitive(self) -> {{ .Type }} {
		self as {{ .Type }}
	}

	#[deprecated = "Strict enums should not use validate()"]
	#[inline]
	pub fn validate(self) -> std::result::Result<Self, {{ .Type }}> {
		Ok(self)
	}

	#[deprecated = "Strict enums should not use is_unknown()"]
	#[inline]
	pub fn is_unknown(&self) -> bool {
		false
	}
{{- else }}
	#[inline]
	pub fn from_primitive_allow_unknown(prim: {{ .Type }}) -> Self {
		match prim {
			{{- range .Members }}
			{{ .Value }} => Self::{{ .Name }},
			{{- end }}
			#[allow(deprecated)]
			x => Self::__Unknown(x),
		}
	}

	#[inline]
	pub fn unknown() -> Self {
		#[allow(deprecated)]
		Self::__Unknown({{ .UnknownValueForTmpl | printf "%#x" }})
	}

	#[inline]
	pub fn into_primitive(self) -> {{ .Type }} {
		match self {
			{{- range .Members }}
			Self::{{ .Name }} => {{ .Value }},
			{{- end }}
			#[allow(deprecated)]
			Self::__Unknown(x) => x,
		}
	}

	#[inline]
	pub fn validate(self) -> std::result::Result<Self, {{ .Type }}> {
		match self {
			{{- range .Members }}
			{{- if .IsUnknown }}
			Self::{{ .Name }} => Err(self.into_primitive()),
			{{- end }}
			{{- end }}
			#[allow(deprecated)]
			Self::__Unknown(x) => Err(x),
			_ => Ok(self),
		}
	}

	#[inline]
	pub fn is_unknown(&self) -> bool {
		self.validate().is_err()
	}
{{- end }}
}

fidl_enum! {
	name: {{ .Name }},
	prim_ty: {{ .Type }},
	{{- if .IsStrict }}
	strict: true,
	min_member: {{ .MinMember }},
	{{- else }}
	flexible: true,
	{{- end }}
}
{{ end }}
`
