// SPDX-License-Identifier: GPL-2.0-or-later
// The phone's ongoing items (timers, downloads, navigation) as text. Pure:
// no GNOME imports, so it can be tested with node.

/// "4:32", or "1:05:09" from an hour; a countdown rounds up.
export function clockText(ms) {
    const seconds = Math.ceil(Math.max(0, ms) / 1000);
    const h = Math.floor(seconds / 3600), m = Math.floor(seconds / 60) % 60, s = seconds % 60;
    const two = n => String(n).padStart(2, '0');
    return h > 0 ? `${h}:${two(m)}:${two(s)}` : `${m}:${two(s)}`;
}

/// The live part of an ongoing item ("4:32 left", "0:12 elapsed", "42%"), or
/// '' when it has none. `base` is on this computer's clock.
export function liveText(item, now) {
    if (item.base > 0) {
        if (item.countdown)
            return item.base - now <= 0 ? 'Done' : `${clockText(item.base - now)} left`;
        return `${clockText(now - item.base)} elapsed`;
    }
    return item.progress >= 0 ? `${item.progress}%` : '';
}

/// The items still worth showing: a countdown over for a minute is not.
export function liveItems(items, now) {
    return items.filter(item => !(item.countdown && item.base > 0 && now - item.base > 60000));
}
