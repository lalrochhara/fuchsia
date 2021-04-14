// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package codegen

const fragmentMethodUnownedResultTmpl = `
{{- define "MethodUnownedResultDeclaration" }}
{{- EnsureNamespace "" }}
template<>
class {{ .WireUnownedResult }} final : public ::fidl::Result {
	public:
	 explicit {{ .WireUnownedResult.Self }}(
		 ::fidl::UnownedClientEnd<{{ .Protocol }}> _client
	   {{- if .RequestArgs -}}
		 , uint8_t* _request_bytes, uint32_t _request_byte_capacity
	   {{- end -}}
	   {{- .RequestArgs | CalleeCommaParams }}
	   {{- if .HasResponse -}}
		 , uint8_t* _response_bytes, uint32_t _response_byte_capacity
	   {{- end -}});
	 explicit {{ .WireUnownedResult.Self }}(const ::fidl::Result& result) : ::fidl::Result(result) {}
	 {{ .WireUnownedResult.Self }}({{ .WireUnownedResult.Self }}&&) = delete;
	 {{ .WireUnownedResult.Self }}(const {{ .WireUnownedResult.Self }}&) = delete;
	 {{ .WireUnownedResult.Self }}* operator=({{ .WireUnownedResult.Self }}&&) = delete;
	 {{ .WireUnownedResult.Self }}* operator=(const {{ .WireUnownedResult.Self }}&) = delete;
	 {{- if and .HasResponse .Response.IsResource }}
	 ~{{ .WireUnownedResult.Self }}() {
	   if (ok()) {
		 Unwrap()->_CloseHandles();
	   }
	 }
	 {{- else }}
	 ~{{ .WireUnownedResult.Self }}() = default;
	 {{- end }}
	 {{- if .HasResponse }}

	 {{ .WireResponse }}* Unwrap() {
	   ZX_DEBUG_ASSERT(ok());
	   return reinterpret_cast<{{ .WireResponse }}*>(bytes_);
	 }
	 const {{ .WireResponse }}* Unwrap() const {
	   ZX_DEBUG_ASSERT(ok());
	   return reinterpret_cast<const {{ .WireResponse }}*>(bytes_);
	 }

	 {{ .WireResponse }}& value() { return *Unwrap(); }
	 const {{ .WireResponse }}& value() const { return *Unwrap(); }

	 {{ .WireResponse }}* operator->() { return &value(); }
	 const {{ .WireResponse }}* operator->() const { return &value(); }

	 {{ .WireResponse }}& operator*() { return value(); }
	 const {{ .WireResponse }}& operator*() const { return value(); }

	private:
	 uint8_t* bytes_;
	 {{- end }}
};
{{- end }}




{{- define "MethodUnownedResultDefinition" }}
{{- IfdefFuchsia -}}
{{- EnsureNamespace "" }}
{{ .WireUnownedResult }}::{{ .WireUnownedResult.Self }}(
  ::fidl::UnownedClientEnd<{{ .Protocol }}> _client
  {{- if .RequestArgs -}}
  , uint8_t* _request_bytes, uint32_t _request_byte_capacity
  {{- end -}}
  {{- .RequestArgs | CalleeCommaParams }}
  {{- if .HasResponse }}
  , uint8_t* _response_bytes, uint32_t _response_byte_capacity)
    : bytes_(_response_bytes) {
  {{- else }}
  ) {
  {{- end }}
  {{- if .RequestArgs -}}
  ::fidl::UnownedEncodedMessage<{{ .WireRequest }}> _request(
      _request_bytes, _request_byte_capacity, 0
  {{- else -}}
  ::fidl::OwnedEncodedMessage<{{ .WireRequest }}> _request(zx_txid_t(0)
  {{- end -}}
    {{- .RequestArgs | ForwardCommaParams -}});
  {{- if .HasResponse }}
  _request.GetOutgoingMessage().Call<{{ .WireResponse }}>(_client, _response_bytes,
                                                          _response_byte_capacity);
  {{- else }}
  _request.GetOutgoingMessage().Write(_client);
  {{- end }}
  status_ = _request.status();
  error_ = _request.error();
}
{{- EndifFuchsia -}}
{{- end }}
`
