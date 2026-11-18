package dev.mnemonic.phone

object Fixtures {
    fun wire(name: String): String =
        checkNotNull(Fixtures::class.java.classLoader?.getResource("wire/$name")) {
            "missing fixture wire/$name"
        }.readText()
}
