package dev.mnemonic.phone.watch

import android.content.Context
import android.media.MediaRecorder
import android.os.Build
import android.util.Log
import java.io.File

/**
 * Records from the phone's current microphone, which is a plugged-in
 * lavalier when there is one, until told to finish, so pauses don't cut it
 * short. Works with the screen off only while the watch service holds its
 * microphone foreground type.
 */
class Recording private constructor(private val recorder: MediaRecorder, private val file: File) {
    /** The recording as AAC in MP4, or null when nothing was captured. */
    fun finish(): ByteArray? {
        try {
            recorder.stop()
        } catch (error: RuntimeException) {
            // Thrown when nothing was recorded, or the time limit already stopped it.
            Log.w(TAG, "Stopping the recording failed", error)
        }
        recorder.release()
        val audio = file.takeIf { it.length() > 0 }?.readBytes()
        file.delete()
        return audio
    }

    fun cancel() {
        recorder.release()
        file.delete()
    }

    companion object {
        private const val TAG = "Recording"
        const val MAX_MILLIS = 15 * 60 * 1000

        /** Throws when the microphone can't be opened, as when another app holds it. */
        fun start(context: Context, onLimit: () -> Unit): Recording {
            val file = File.createTempFile("watch-", ".m4a", context.cacheDir)
            val recorder = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                MediaRecorder(context)
            } else {
                @Suppress("DEPRECATION")
                MediaRecorder()
            }
            try {
                recorder.setAudioSource(MediaRecorder.AudioSource.MIC)
                recorder.setOutputFormat(MediaRecorder.OutputFormat.MPEG_4)
                recorder.setAudioEncoder(MediaRecorder.AudioEncoder.AAC)
                recorder.setAudioChannels(1)
                recorder.setAudioSamplingRate(16_000)
                recorder.setAudioEncodingBitRate(32_000)
                recorder.setMaxDuration(MAX_MILLIS)
                recorder.setOnInfoListener { _, what, _ ->
                    if (what == MediaRecorder.MEDIA_RECORDER_INFO_MAX_DURATION_REACHED) onLimit()
                }
                recorder.setOutputFile(file)
                recorder.prepare()
                recorder.start()
            } catch (error: Exception) {
                recorder.release()
                file.delete()
                throw error
            }
            return Recording(recorder, file)
        }
    }
}
