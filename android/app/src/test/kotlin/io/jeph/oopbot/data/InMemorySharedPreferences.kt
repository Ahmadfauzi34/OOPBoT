package io.jeph.oopbot.data

import android.content.SharedPreferences

/** Lightweight in-memory [SharedPreferences] implementation for pure JVM unit tests without Robolectric. */
internal class InMemorySharedPreferences(initial: Map<String, Any?> = emptyMap()) : SharedPreferences {
  private val data = LinkedHashMap<String, Any?>(initial)
  private val listeners = LinkedHashSet<SharedPreferences.OnSharedPreferenceChangeListener>()

  fun keys(): Set<String> = synchronized(data) { data.keys.toSet() }

  override fun getAll(): MutableMap<String, *> = synchronized(data) { LinkedHashMap(data) }

  override fun getString(key: String?, defValue: String?): String? =
    synchronized(data) { (data[key] as? String) ?: defValue }

  override fun getStringSet(key: String?, defValues: MutableSet<String>?): MutableSet<String>? = synchronized(data) {
    @Suppress("UNCHECKED_CAST")
    (data[key] as? Set<String>)?.let { LinkedHashSet(it) } ?: defValues
  }

  override fun getInt(key: String?, defValue: Int): Int = synchronized(data) { (data[key] as? Int) ?: defValue }

  override fun getLong(key: String?, defValue: Long): Long = synchronized(data) { (data[key] as? Long) ?: defValue }

  override fun getFloat(key: String?, defValue: Float): Float = synchronized(data) { (data[key] as? Float) ?: defValue }

  override fun getBoolean(key: String?, defValue: Boolean): Boolean =
    synchronized(data) { (data[key] as? Boolean) ?: defValue }

  override fun contains(key: String?): Boolean = synchronized(data) { key in data }

  override fun edit(): SharedPreferences.Editor = InMemoryEditor()

  override fun registerOnSharedPreferenceChangeListener(listener: SharedPreferences.OnSharedPreferenceChangeListener?) {
    if (listener != null) synchronized(listeners) { listeners += listener }
  }

  override fun unregisterOnSharedPreferenceChangeListener(
    listener: SharedPreferences.OnSharedPreferenceChangeListener?,
  ) {
    if (listener != null) synchronized(listeners) { listeners -= listener }
  }

  private inner class InMemoryEditor : SharedPreferences.Editor {
    private val pending = LinkedHashMap<String, Any?>()
    private val removals = LinkedHashSet<String>()
    private var clearAll = false

    override fun putString(key: String?, value: String?): SharedPreferences.Editor = apply {
      if (key != null) {
        if (value == null) removals += key else pending[key] = value
      }
    }

    override fun putStringSet(key: String?, values: MutableSet<String>?): SharedPreferences.Editor = apply {
      if (key != null) {
        if (values == null) removals += key else pending[key] = LinkedHashSet(values)
      }
    }

    override fun putInt(key: String?, value: Int): SharedPreferences.Editor = apply {
      if (key != null) pending[key] = value
    }

    override fun putLong(key: String?, value: Long): SharedPreferences.Editor = apply {
      if (key != null) pending[key] = value
    }

    override fun putFloat(key: String?, value: Float): SharedPreferences.Editor = apply {
      if (key != null) pending[key] = value
    }

    override fun putBoolean(key: String?, value: Boolean): SharedPreferences.Editor = apply {
      if (key != null) pending[key] = value
    }

    override fun remove(key: String?): SharedPreferences.Editor = apply {
      if (key != null) {
        pending.remove(key)
        removals += key
      }
    }

    override fun clear(): SharedPreferences.Editor = apply {
      clearAll = true
      pending.clear()
      removals.clear()
    }

    override fun commit(): Boolean {
      apply()
      return true
    }

    override fun apply() {
      val changed = synchronized(data) {
        val touched = LinkedHashSet<String>()
        if (clearAll) {
          touched += data.keys
          data.clear()
        }
        for (key in removals) {
          if (key in data) touched += key
          data.remove(key)
        }
        for ((key, value) in pending) {
          if (data[key] != value) touched += key
          data[key] = value
        }
        touched
      }
      if (changed.isNotEmpty()) {
        val snapshot = synchronized(listeners) { listeners.toList() }
        for (listener in snapshot) {
          for (key in changed) listener.onSharedPreferenceChanged(this@InMemorySharedPreferences, key)
        }
      }
    }
  }
}
