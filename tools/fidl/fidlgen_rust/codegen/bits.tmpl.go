// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package codegen

const bitsTmpl = `
{{- define "BitsDeclaration" -}}
bitflags! {
	{{- range .DocComments}}
	///{{ . }}
	{{- end}}
	pub struct {{ .Name }}: {{ .Type }} {
		{{- range .Members }}
		{{- range .DocComments}}
		///{{ . }}
		{{- end}}
		const {{ .Name }} = {{ .Value }};
		{{- end }}
	}
}

impl {{ .Name }} {
{{- if .IsStrict }}
	#[deprecated = "Strict bits should not use has_unknown_bits()"]
	#[inline(always)]
	pub fn has_unknown_bits(&self) -> bool {
		false
	}

	#[deprecated = "Strict bits should not use get_unknown_bits()"]
	#[inline(always)]
	pub fn get_unknown_bits(&self) -> {{ .Type }} {
		0
	}
{{- else }}
	#[inline(always)]
	pub fn from_bits_allow_unknown(bits: {{ .Type }}) -> Self {
		unsafe { Self::from_bits_unchecked(bits) }
	}

	#[inline(always)]
	pub fn has_unknown_bits(&self) -> bool {
		self.get_unknown_bits() != 0
	}

	#[inline(always)]
	pub fn get_unknown_bits(&self) -> {{ .Type }} {
		self.bits & !Self::all().bits
	}
{{- end }}
}

fidl_bits! {
	name: {{ .Name }},
	prim_ty: {{ .Type }},
	{{- if .IsStrict }}
	strict: true,
	{{- else }}
	flexible: true,
	{{- end }}
}
{{ end }}
`
