package dev.mnemonic.phone.api

import okhttp3.HttpUrl
import okhttp3.HttpUrl.Companion.toHttpUrlOrNull

data class MacAddress(val baseUrl: String, val origin: String) {
    companion object {
        fun parse(text: String): MacAddress? {
            val trimmed = text.trim()
            if (trimmed.isEmpty()) return null
            val withScheme = if ("://" in trimmed) trimmed else "https://$trimmed"
            val url = withScheme.toHttpUrlOrNull() ?: return null
            val host = if (':' in url.host) "[${url.host}]" else url.host
            val port = if (url.port == HttpUrl.defaultPort(url.scheme)) "" else ":${url.port}"
            val origin = "${url.scheme}://$host$port"
            return MacAddress(baseUrl = "$origin/", origin = origin)
        }
    }
}
