// AWS SigV4 request signing for the Iceberg extension on wasm32-wasi.
//
// The upstream aws.cpp signs catalog requests with the AWS C++ SDK
// (Aws::Client::AWSAuthV4Signer) and sends them with Aws::Http. Neither is
// available on wasi, so this file (injected by scripts/build-libduckdb-wasm.sh
// into aws.cpp's `#ifdef __wasi__` branch, inside `namespace duckdb`) provides
// AWSInput::{Get,Head,Delete,Post}Request: a self-contained SigV4 signer
// (SHA-256 + HMAC, no external crypto) that issues the signed request through
// DuckDB's HTTPUtil (the curl client on wasi). Enables AWS-native Iceberg
// catalogs (Glue / S3 Tables) without the AWS SDK.

namespace {

// --- SHA-256 (FIPS 180-4) -------------------------------------------------
struct WasiSha256 {
	uint32_t h[8];
	uint64_t len;
	uint8_t buf[64];
	size_t buf_len;
	WasiSha256() : len(0), buf_len(0) {
		static const uint32_t iv[8] = {0x6a09e667u, 0xbb67ae85u, 0x3c6ef372u, 0xa54ff53au,
		                               0x510e527fu, 0x9b05688cu, 0x1f83d9abu, 0x5be0cd19u};
		for (int i = 0; i < 8; i++) {
			h[i] = iv[i];
		}
	}
	static uint32_t ror(uint32_t x, uint32_t n) {
		return (x >> n) | (x << (32 - n));
	}
	void block(const uint8_t *p) {
		static const uint32_t k[64] = {
		    0x428a2f98u, 0x71374491u, 0xb5c0fbcfu, 0xe9b5dba5u, 0x3956c25bu, 0x59f111f1u, 0x923f82a4u, 0xab1c5ed5u,
		    0xd807aa98u, 0x12835b01u, 0x243185beu, 0x550c7dc3u, 0x72be5d74u, 0x80deb1feu, 0x9bdc06a7u, 0xc19bf174u,
		    0xe49b69c1u, 0xefbe4786u, 0x0fc19dc6u, 0x240ca1ccu, 0x2de92c6fu, 0x4a7484aau, 0x5cb0a9dcu, 0x76f988dau,
		    0x983e5152u, 0xa831c66du, 0xb00327c8u, 0xbf597fc7u, 0xc6e00bf3u, 0xd5a79147u, 0x06ca6351u, 0x14292967u,
		    0x27b70a85u, 0x2e1b2138u, 0x4d2c6dfcu, 0x53380d13u, 0x650a7354u, 0x766a0abbu, 0x81c2c92eu, 0x92722c85u,
		    0xa2bfe8a1u, 0xa81a664bu, 0xc24b8b70u, 0xc76c51a3u, 0xd192e819u, 0xd6990624u, 0xf40e3585u, 0x106aa070u,
		    0x19a4c116u, 0x1e376c08u, 0x2748774cu, 0x34b0bcb5u, 0x391c0cb3u, 0x4ed8aa4au, 0x5b9cca4fu, 0x682e6ff3u,
		    0x748f82eeu, 0x78a5636fu, 0x84c87814u, 0x8cc70208u, 0x90befffau, 0xa4506cebu, 0xbef9a3f7u, 0xc67178f2u};
		uint32_t w[64];
		for (int i = 0; i < 16; i++) {
			w[i] = (uint32_t(p[i * 4]) << 24) | (uint32_t(p[i * 4 + 1]) << 16) | (uint32_t(p[i * 4 + 2]) << 8) |
			       uint32_t(p[i * 4 + 3]);
		}
		for (int i = 16; i < 64; i++) {
			uint32_t s0 = ror(w[i - 15], 7) ^ ror(w[i - 15], 18) ^ (w[i - 15] >> 3);
			uint32_t s1 = ror(w[i - 2], 17) ^ ror(w[i - 2], 19) ^ (w[i - 2] >> 10);
			w[i] = w[i - 16] + s0 + w[i - 7] + s1;
		}
		uint32_t a = h[0], b = h[1], c = h[2], d = h[3], e = h[4], f = h[5], g = h[6], hh = h[7];
		for (int i = 0; i < 64; i++) {
			uint32_t S1 = ror(e, 6) ^ ror(e, 11) ^ ror(e, 25);
			uint32_t ch = (e & f) ^ (~e & g);
			uint32_t t1 = hh + S1 + ch + k[i] + w[i];
			uint32_t S0 = ror(a, 2) ^ ror(a, 13) ^ ror(a, 22);
			uint32_t maj = (a & b) ^ (a & c) ^ (b & c);
			uint32_t t2 = S0 + maj;
			hh = g; g = f; f = e; e = d + t1; d = c; c = b; b = a; a = t1 + t2;
		}
		h[0] += a; h[1] += b; h[2] += c; h[3] += d; h[4] += e; h[5] += f; h[6] += g; h[7] += hh;
	}
	void update(const uint8_t *data, size_t n) {
		len += n;
		while (n > 0) {
			size_t take = 64 - buf_len;
			if (take > n) {
				take = n;
			}
			for (size_t i = 0; i < take; i++) {
				buf[buf_len + i] = data[i];
			}
			buf_len += take;
			data += take;
			n -= take;
			if (buf_len == 64) {
				block(buf);
				buf_len = 0;
			}
		}
	}
	void final(uint8_t out[32]) {
		uint64_t bits = len * 8;
		uint8_t pad = 0x80;
		update(&pad, 1);
		uint8_t zero = 0;
		while (buf_len != 56) {
			update(&zero, 1);
		}
		uint8_t lenbuf[8];
		for (int i = 0; i < 8; i++) {
			lenbuf[i] = uint8_t(bits >> (56 - i * 8));
		}
		update(lenbuf, 8);
		for (int i = 0; i < 8; i++) {
			out[i * 4] = uint8_t(h[i] >> 24);
			out[i * 4 + 1] = uint8_t(h[i] >> 16);
			out[i * 4 + 2] = uint8_t(h[i] >> 8);
			out[i * 4 + 3] = uint8_t(h[i]);
		}
	}
};

void Sha256Raw(const uint8_t *data, size_t n, uint8_t out[32]) {
	WasiSha256 s;
	s.update(data, n);
	s.final(out);
}

string ToHex(const uint8_t *d, size_t n) {
	static const char *hx = "0123456789abcdef";
	string r;
	r.reserve(n * 2);
	for (size_t i = 0; i < n; i++) {
		r.push_back(hx[d[i] >> 4]);
		r.push_back(hx[d[i] & 0xf]);
	}
	return r;
}

string Sha256Hex(const string &data) {
	uint8_t out[32];
	Sha256Raw(reinterpret_cast<const uint8_t *>(data.data()), data.size(), out);
	return ToHex(out, 32);
}

// HMAC-SHA256 (block size 64).
void HmacSha256(const uint8_t *key, size_t klen, const string &msg, uint8_t out[32]) {
	uint8_t k[64];
	if (klen > 64) {
		Sha256Raw(key, klen, k);
		for (int i = 32; i < 64; i++) {
			k[i] = 0;
		}
	} else {
		for (size_t i = 0; i < klen; i++) {
			k[i] = key[i];
		}
		for (size_t i = klen; i < 64; i++) {
			k[i] = 0;
		}
	}
	uint8_t ipad[64], opad[64];
	for (int i = 0; i < 64; i++) {
		ipad[i] = k[i] ^ 0x36;
		opad[i] = k[i] ^ 0x5c;
	}
	WasiSha256 inner;
	inner.update(ipad, 64);
	inner.update(reinterpret_cast<const uint8_t *>(msg.data()), msg.size());
	uint8_t ih[32];
	inner.final(ih);
	WasiSha256 outer;
	outer.update(opad, 64);
	outer.update(ih, 32);
	outer.final(out);
}

// RFC 3986 encoding. AWS encodes everything except A-Za-z0-9 -_.~ ; the path
// keeps '/' (encode_slash=false), query keys/values encode it.
string UriEncode(const string &s, bool encode_slash) {
	static const char *hx = "0123456789ABCDEF";
	string r;
	for (unsigned char c : s) {
		if ((c >= 'A' && c <= 'Z') || (c >= 'a' && c <= 'z') || (c >= '0' && c <= '9') || c == '-' || c == '_' ||
		    c == '.' || c == '~') {
			r.push_back(char(c));
		} else if (c == '/' && !encode_slash) {
			r.push_back('/');
		} else {
			r.push_back('%');
			r.push_back(hx[c >> 4]);
			r.push_back(hx[c & 0xf]);
		}
	}
	return r;
}

struct SignedRequest {
	string url;
	string host;
	string amz_date;
	string authorization;
	string payload_hash;
};

// Build the canonical request, string-to-sign, and Authorization header.
SignedRequest SignV4(const AWSInput &in, const string &method, const string &body) {
	// in.authority may include the scheme (the IRC endpoint builder keeps it);
	// the signed Host header + the request URL use the bare host:port.
	string scheme = "https";
	string host = in.authority;
	if (host.rfind("https://", 0) == 0) {
		host = host.substr(8);
	} else if (host.rfind("http://", 0) == 0) {
		scheme = "http";
		host = host.substr(7);
	}

	// Timestamps (UTC): YYYYMMDDTHHMMSSZ and YYYYMMDD.
	time_t now = time(nullptr);
	struct tm gmt;
	gmtime_r(&now, &gmt);
	char amz[32], ds[16];
	strftime(amz, sizeof(amz), "%Y%m%dT%H%M%SZ", &gmt);
	strftime(ds, sizeof(ds), "%Y%m%d", &gmt);
	string amz_date = amz, datestamp = ds;

	// Canonical URI (path segments, each RFC3986-encoded, '/'-joined).
	string canonical_uri = "/";
	for (idx_t i = 0; i < in.path_segments.size(); i++) {
		if (i) {
			canonical_uri += "/";
		}
		canonical_uri += UriEncode(in.path_segments[i], true);
	}

	// Canonical query string (sorted by key, key+value encoded).
	vector<std::pair<string, string>> qp;
	for (auto &p : in.query_string_parameters) {
		qp.emplace_back(UriEncode(p.first, true), UriEncode(p.second, true));
	}
	std::sort(qp.begin(), qp.end());
	string canonical_qs;
	for (idx_t i = 0; i < qp.size(); i++) {
		if (i) {
			canonical_qs += "&";
		}
		canonical_qs += qp[i].first + "=" + qp[i].second;
	}

	string payload_hash = Sha256Hex(body);

	// Canonical headers (host, x-amz-content-sha256, x-amz-date[, token]) sorted.
	vector<std::pair<string, string>> hdrs;
	hdrs.emplace_back("host", host);
	hdrs.emplace_back("x-amz-content-sha256", payload_hash);
	hdrs.emplace_back("x-amz-date", amz_date);
	if (!in.session_token.empty()) {
		hdrs.emplace_back("x-amz-security-token", in.session_token);
	}
	std::sort(hdrs.begin(), hdrs.end());
	string canonical_headers, signed_headers;
	for (idx_t i = 0; i < hdrs.size(); i++) {
		canonical_headers += hdrs[i].first + ":" + hdrs[i].second + "\n";
		if (i) {
			signed_headers += ";";
		}
		signed_headers += hdrs[i].first;
	}

	string canonical_request = method + "\n" + canonical_uri + "\n" + canonical_qs + "\n" + canonical_headers + "\n" +
	                           signed_headers + "\n" + payload_hash;

	string scope = datestamp + "/" + in.region + "/" + in.service + "/aws4_request";
	string string_to_sign =
	    "AWS4-HMAC-SHA256\n" + amz_date + "\n" + scope + "\n" + Sha256Hex(canonical_request);

	// Signing key: HMAC chain seeded with "AWS4"+secret.
	string k0 = "AWS4" + in.secret;
	uint8_t kDate[32], kRegion[32], kService[32], kSigning[32], sig[32];
	HmacSha256(reinterpret_cast<const uint8_t *>(k0.data()), k0.size(), datestamp, kDate);
	HmacSha256(kDate, 32, in.region, kRegion);
	HmacSha256(kRegion, 32, in.service, kService);
	HmacSha256(kService, 32, string("aws4_request"), kSigning);
	HmacSha256(kSigning, 32, string_to_sign, sig);
	string signature = ToHex(sig, 32);

	SignedRequest out;
	out.host = host;
	out.amz_date = amz_date;
	out.payload_hash = payload_hash;
	out.authorization = "AWS4-HMAC-SHA256 Credential=" + in.key_id + "/" + scope + ", SignedHeaders=" +
	                    signed_headers + ", Signature=" + signature;
	out.url = scheme + "://" + host + canonical_uri + (canonical_qs.empty() ? "" : "?" + canonical_qs);
	return out;
}

unique_ptr<HTTPResponse> ExecuteSigned(ClientContext &context, AWSInput &in, const string &method,
                                       const string &body, const string &content_type) {
	auto signed_req = SignV4(in, method, body);
	auto &db = DatabaseInstance::GetDatabase(context);
	HTTPHeaders headers(db);
	headers.Insert("Authorization", signed_req.authorization);
	headers.Insert("x-amz-date", signed_req.amz_date);
	headers.Insert("x-amz-content-sha256", signed_req.payload_hash);
	headers.Insert("host", signed_req.host);
	if (!in.session_token.empty()) {
		headers.Insert("x-amz-security-token", in.session_token);
	}
	if (!in.user_agent.empty()) {
		headers.Insert("User-Agent", in.user_agent);
	}
	if (!body.empty() && !content_type.empty()) {
		headers.Insert("Content-Type", content_type);
	}
	auto &http_util = HTTPUtil::Get(db);
	auto params = http_util.InitializeParameters(context, signed_req.url);
	if (method == "GET") {
		GetRequestInfo req(signed_req.url, headers, *params, nullptr, nullptr);
		return http_util.Request(req);
	} else if (method == "HEAD") {
		HeadRequestInfo req(signed_req.url, headers, *params);
		return http_util.Request(req);
	} else if (method == "DELETE") {
		DeleteRequestInfo req(signed_req.url, headers, *params);
		return http_util.Request(req);
	}
	PostRequestInfo req(signed_req.url, headers, *params, reinterpret_cast<const_data_ptr_t>(body.data()),
	                    body.size());
	auto resp = http_util.Request(req);
	if (resp && resp->body.empty()) {
		resp->body = req.buffer_out;
	}
	return resp;
}

} // namespace

unique_ptr<HTTPResponse> AWSInput::GetRequest(ClientContext &context) {
	return ExecuteSigned(context, *this, "GET", "", "");
}
unique_ptr<HTTPResponse> AWSInput::HeadRequest(ClientContext &context) {
	return ExecuteSigned(context, *this, "HEAD", "", "");
}
unique_ptr<HTTPResponse> AWSInput::DeleteRequest(ClientContext &context) {
	return ExecuteSigned(context, *this, "DELETE", "", "");
}
unique_ptr<HTTPResponse> AWSInput::PostRequest(ClientContext &context, string post_body) {
	return ExecuteSigned(context, *this, "POST", post_body, "application/json");
}
