package io.github.mthli.appine

import android.app.Application
import android.app.Service
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.Binder
import android.os.IBinder
import android.os.Parcel
import android.os.ParcelFileDescriptor
import android.util.Log
import java.util.concurrent.Executor
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.concurrent.thread

private const val DESCRIPTOR = "io.github.mthli.appine.GpuService"
private const val CONNECT = IBinder.FIRST_CALL_TRANSACTION
private const val TAG = "appine-venus"

/** Private GPU process. Each connected socket owns one Venus context. */
internal class GpuService : Service() {

    override fun onBind(intent: Intent): IBinder = object : Binder() {
        override fun onTransact(code: Int, data: Parcel, reply: Parcel?, flags: Int): Boolean {
            if (code != CONNECT) return super.onTransact(code, data, reply, flags)
            data.enforceInterface(DESCRIPTOR)
            val endpoint = ParcelFileDescriptor.CREATOR.createFromParcel(data)
            thread(name = "appine-venus") {
                System.loadLibrary("appine")
                runWorker(endpoint.detachFd())
            }
            return true
        }
    }

    companion object {
        private val direct = Executor { it.run() }
        lateinit var app: Application

        @JvmStatic private external fun runWorker(fd: Int)

        @JvmStatic fun connect(fd: Int): GpuConnection? {
            val endpoint = ParcelFileDescriptor.fromFd(fd)
            val connection = GpuConnection(app, endpoint)
            try {
                val intent = Intent(app, GpuService::class.java)
                if (app.bindService(intent, Context.BIND_AUTO_CREATE, direct, connection)) return connection
            } catch (e: RuntimeException) {
                Log.w(TAG, "Cannot bind renderer", e)
            }
            endpoint.close()
            return null
        }
    }
}

internal class GpuConnection(
    private val app: Context,
    private val endpoint: ParcelFileDescriptor,
) : ServiceConnection {
    private val closed = AtomicBoolean()

    override fun onServiceConnected(name: ComponentName, service: IBinder) {
        if (closed.get()) return
        val parcel = Parcel.obtain()
        try {
            parcel.writeInterfaceToken(DESCRIPTOR)
            endpoint.writeToParcel(parcel, 0)
            service.transact(CONNECT, parcel, null, IBinder.FLAG_ONEWAY)
        } catch (e: Exception) {
            Log.w(TAG, "Cannot pass renderer socket", e)
        } finally {
            endpoint.close()
            parcel.recycle()
        }
    }

    override fun onNullBinding(name: ComponentName) = close()
    override fun onBindingDied(name: ComponentName) = close()
    override fun onServiceDisconnected(name: ComponentName) = close()

    fun close() {
        if (closed.compareAndSet(false, true)) {
            endpoint.close()
            app.unbindService(this)
        }
    }
}
