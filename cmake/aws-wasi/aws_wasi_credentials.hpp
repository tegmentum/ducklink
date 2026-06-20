//===----------------------------------------------------------------------===//
// aws_wasi_credentials.hpp
//
// wasm-native replacement for the AWS C++ SDK's credential-provider chain. The
// SDK does not build for wasm32-wasip2, but the `aws` extension only needs it to
// resolve credentials + region from the standard NON-NETWORK sources, which are
// trivial to read directly:
//   - environment variables (AWS_ACCESS_KEY_ID / SECRET / SESSION_TOKEN, region,
//     profile, AWS_SHARED_CREDENTIALS_FILE / AWS_CONFIG_FILE)
//   - the INI credentials file (~/.aws/credentials) and config file (~/.aws/config)
// The network/subprocess providers (sso / sts / instance / process) need HTTP or
// a child process and are reported as unsupported on wasm.
//
// Header-only so it doesn't need a new CMake source entry.
//===----------------------------------------------------------------------===//
#pragma once

#ifdef __wasi__

#include "duckdb/common/exception.hpp"
#include "duckdb/common/string_util.hpp"

#include <cstdio>
#include <cstdlib>
#include <map>

namespace duckdb {
namespace aws_wasi {

struct Credentials {
	string access_key_id;
	string secret_access_key;
	string session_token;
	bool IsEmpty() const {
		return access_key_id.empty() || secret_access_key.empty();
	}
};

// section -> (key -> value)
using IniFile = std::map<string, std::map<string, string>>;

inline string GetEnv(const char *name) {
	const char *v = std::getenv(name);
	return v ? string(v) : string();
}

inline string TrimWs(const string &s) {
	idx_t start = 0;
	while (start < s.size() && std::isspace((unsigned char)s[start])) {
		start++;
	}
	idx_t end = s.size();
	while (end > start && std::isspace((unsigned char)s[end - 1])) {
		end--;
	}
	return s.substr(start, end - start);
}

inline IniFile ParseIni(const string &path) {
	IniFile result;
	FILE *f = std::fopen(path.c_str(), "rb");
	if (!f) {
		return result; // missing file -> empty (caller treats as "no creds here")
	}
	string current_section;
	char buf[4096];
	while (std::fgets(buf, sizeof(buf), f)) {
		string line = TrimWs(string(buf));
		if (line.empty() || line[0] == '#' || line[0] == ';') {
			continue;
		}
		if (line.front() == '[' && line.back() == ']') {
			current_section = TrimWs(line.substr(1, line.size() - 2));
			continue;
		}
		auto eq = line.find('=');
		if (eq == string::npos || current_section.empty()) {
			continue;
		}
		string key = StringUtil::Lower(TrimWs(line.substr(0, eq)));
		string value = TrimWs(line.substr(eq + 1));
		result[current_section][key] = value;
	}
	std::fclose(f);
	return result;
}

//! Resolve the active profile name (param > AWS_PROFILE > AWS_DEFAULT_PROFILE > "default").
inline string ResolveProfileName(const string &profile_param) {
	if (!profile_param.empty()) {
		return profile_param;
	}
	auto env_profile = GetEnv("AWS_PROFILE");
	if (!env_profile.empty()) {
		return env_profile;
	}
	env_profile = GetEnv("AWS_DEFAULT_PROFILE");
	return env_profile.empty() ? string("default") : env_profile;
}

inline string CredentialsFilePath() {
	auto override_path = GetEnv("AWS_SHARED_CREDENTIALS_FILE");
	if (!override_path.empty()) {
		return override_path;
	}
	auto home = GetEnv("HOME");
	return home.empty() ? string() : home + "/.aws/credentials";
}

inline string ConfigFilePath() {
	auto override_path = GetEnv("AWS_CONFIG_FILE");
	if (!override_path.empty()) {
		return override_path;
	}
	auto home = GetEnv("HOME");
	return home.empty() ? string() : home + "/.aws/config";
}

//! In ~/.aws/config, non-default profiles live under "[profile NAME]" (the
//! credentials file uses "[NAME]" directly).
inline const std::map<string, string> *FindProfileSection(const IniFile &ini, const string &profile,
                                                          bool config_style) {
	if (config_style && profile != "default") {
		auto it = ini.find("profile " + profile);
		if (it != ini.end()) {
			return &it->second;
		}
	}
	auto it = ini.find(profile);
	return it == ini.end() ? nullptr : &it->second;
}

inline Credentials FromEnvironment() {
	Credentials creds;
	creds.access_key_id = GetEnv("AWS_ACCESS_KEY_ID");
	creds.secret_access_key = GetEnv("AWS_SECRET_ACCESS_KEY");
	creds.session_token = GetEnv("AWS_SESSION_TOKEN");
	if (creds.session_token.empty()) {
		creds.session_token = GetEnv("AWS_SECURITY_TOKEN"); // legacy name
	}
	return creds;
}

inline string LookupKey(const std::map<string, string> *section, const string &key) {
	if (!section) {
		return string();
	}
	auto it = section->find(key);
	return it == section->end() ? string() : it->second;
}

//! Read static credentials from the shared credentials file, then the config file.
inline Credentials FromProfileFiles(const string &profile) {
	Credentials creds;
	auto cred_path = CredentialsFilePath();
	if (!cred_path.empty()) {
		auto ini = ParseIni(cred_path);
		auto *section = FindProfileSection(ini, profile, /*config_style=*/false);
		creds.access_key_id = LookupKey(section, "aws_access_key_id");
		creds.secret_access_key = LookupKey(section, "aws_secret_access_key");
		creds.session_token = LookupKey(section, "aws_session_token");
	}
	if (creds.IsEmpty()) {
		auto config_path = ConfigFilePath();
		if (!config_path.empty()) {
			auto ini = ParseIni(config_path);
			auto *section = FindProfileSection(ini, profile, /*config_style=*/true);
			creds.access_key_id = LookupKey(section, "aws_access_key_id");
			creds.secret_access_key = LookupKey(section, "aws_secret_access_key");
			creds.session_token = LookupKey(section, "aws_session_token");
		}
	}
	return creds;
}

[[noreturn]] inline void UnsupportedProvider(const string &name) {
	throw NotImplementedException(
	    "AWS '%s' credential provider is not supported on wasm (it needs network or a subprocess). "
	    "Use the 'env' or 'config' providers instead: set AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY "
	    "(and optionally AWS_SESSION_TOKEN) in the environment, or point AWS_SHARED_CREDENTIALS_FILE "
	    "at a preopened credentials file.",
	    name);
}

//! Resolve credentials following the given chain string (";"-separated). Empty
//! chain == the default chain ("env;config" on wasm).
inline Credentials ResolveCredentials(const string &chain, const string &profile) {
	auto chain_str = chain.empty() ? string("env;config") : chain;
	for (auto &raw_item : StringUtil::Split(chain_str, ';')) {
		auto item = StringUtil::Lower(TrimWs(raw_item));
		Credentials creds;
		if (item == "env") {
			creds = FromEnvironment();
		} else if (item == "config" || item == "profile") {
			creds = FromProfileFiles(profile);
		} else if (item == "sso" || item == "sts" || item == "instance" || item == "process") {
			UnsupportedProvider(item);
		} else {
			throw InvalidInputException("Unknown provider in AWS credential chain string: '%s'", item);
		}
		if (!creds.IsEmpty()) {
			return creds;
		}
	}
	return Credentials();
}

//! Region from AWS_REGION / AWS_DEFAULT_REGION, falling back to the profile's
//! `region` in the config (then credentials) file.
inline string ResolveRegion(const string &profile) {
	auto region = GetEnv("AWS_REGION");
	if (!region.empty()) {
		return region;
	}
	region = GetEnv("AWS_DEFAULT_REGION");
	if (!region.empty()) {
		return region;
	}
	auto config_path = ConfigFilePath();
	if (!config_path.empty()) {
		auto ini = ParseIni(config_path);
		region = LookupKey(FindProfileSection(ini, profile, /*config_style=*/true), "region");
		if (!region.empty()) {
			return region;
		}
	}
	auto cred_path = CredentialsFilePath();
	if (!cred_path.empty()) {
		auto ini = ParseIni(cred_path);
		region = LookupKey(FindProfileSection(ini, profile, /*config_style=*/false), "region");
	}
	return region;
}

} // namespace aws_wasi
} // namespace duckdb

#endif // __wasi__
