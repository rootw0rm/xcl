import unittest

from world_load_stall import summarize


def log(time, message, tid=11, level="INFO "):
    return f"[{time:9.3f}][{tid:5}][{level}] {message}\n"


def activity(time, presents, submits=0, draws=0, dispatches=0, requests=0):
    return log(
        time,
        f"activity (2 s): {presents} presents, {submits} submits, {draws} draws, "
        f"{dispatches} dispatches, 0 frame-event waits (0 timed out), "
        f"0 frame statistics (0 not flipped); DirectStorage {requests} requests, 0 MB, 0 failed",
    )


def compile_start(time, number):
    return log(
        time, f"D3D12.X: driver compile start #{number}: compute PSO, 65536 B DXIL, "
        "record fedcba9876543210, driver blob absent", tid=15,
    )


def compile_finish(time, number):
    return log(
        time, f"D3D12.X: driver compile finish #{number}: compute PSO, 65536 B DXIL, 3000 ms",
        tid=15,
    )


def thread_dump(time, *rows, count=None):
    count = len(rows) if count is None else count
    return [log(time, f"thread dump ({count} threads):"), *(row + "\r\n" for row in rows)]


class StallSummaryTests(unittest.TestCase):
    def test_correlates_and_merges_compiles_across_the_window(self):
        report = summarize([
            activity(2, 0),
            activity(4, 20),
            compile_start(5, 10),
            compile_finish(6, 10),
            compile_start(7, 12),
            activity(8, 60),
            compile_start(9, 31),
            compile_finish(11, 12),
            activity(10, 0, 6, 5, 3, 9),
            compile_finish(11, 31),
            compile_start(11.5, 45),
            activity(12, 0, 1, 0, 2, 1),
            activity(14, 0),
            compile_start(15, 59),
        ])

        self.assertIn("Last sample with presents: 8.000 s (60 presents)", report)
        self.assertIn("Sampled window: 8.000 to 14.000 s (6.000 s)", report)
        self.assertIn("7 submits, 5 draws, 5 dispatches, 10 DirectStorage requests", report)
        self.assertIn("Logged driver compiles overlapping the window: 3 (1 began earlier)", report)
        self.assertIn("Matched driver compile overlap: 3.000 s", report)
        self.assertIn("Possible overlap including 1 start without a logged finish: 5.500 s", report)
        self.assertIn("First overlapping compile: #12 compute (65536 B DXIL) at 7.000 s", report)

    def test_aligns_logged_waits_and_excludes_post_stop_activity(self):
        report = summarize([
            activity(8, 55),
            activity(10, 0),
            log(10.1, "D3D12.X: Wait64BitValueX at 0x2000 >= 0x6 bridged to GPU queue.Wait "
                "(waiter=q1/ty0, prod=qNone/tySome(2); wait-before-signal, after_write64=false)"),
            # The bridge for this release may not have been logged.
            log(10.2, "D3D12.X: GPU queue.Wait at 0x1000 >= 0x4 did not complete within 5000 ms "
                "(waiter=q0/ty0, prod=qSome(2)/tySome(2), recorded=0x4, submitted=0x4, completed=0x0)"
                " – released on CPU to prevent GPU hang", level="WARN "),
            activity(12, 0),
            log(13, "XCL_RUN_SECONDS: 13 s – ending the title (60 frames shown, 60 PresentX calls)"),
            activity(14, 30, 10),
            compile_start(13.1, 16),
        ])

        self.assertIn("Sampled window: 8.000 to 12.000 s (4.000 s)", report)
        self.assertIn("Logged driver compiles overlapping the window: 0", report)
        self.assertIn("Logged wait-before-signal bridges: 1 (first at 10.100 s)", report)
        self.assertIn("Logged GPU wait watchdog releases: 1 (first at 10.200 s)", report)

    def test_startup_without_presents_has_no_presentation_loss_window(self):
        report = summarize([activity(2, 0), activity(4, 0)])
        self.assertIn("Activity samples contain no presents", report)

    def test_earlier_pause_ends_when_presents_resume(self):
        report = summarize([
            activity(2, 33),
            activity(4, 0),
            activity(6, 22),
        ])
        self.assertIn("No terminal zero-present activity samples", report)

    def test_no_activity_samples(self):
        report = summarize(["not a runtime log", compile_start(4, 1)])
        self.assertIn("No activity samples found. Capture with XCL_ACTIVITY=1.", report)

    def test_latest_dump_including_closing_sample_matches_windows_module_names(self):
        lines = [
            activity(8, 50),
            activity(10, 0),
            *thread_dump(12, "thread 42 'worker's caller': dxgi.dll+0x1 < XCL.EXE+0xA "
                         "< xcl.exe+0xb < … 19 frames … < GAME.EXE+0xc < xcl.exe+0xd",
                         "thread 43 '': game.exe+0x1 < xcl.exe+0xe",
                         "thread 44 '': " + " < ".join(["xcl.exe+0xf"] * 14),
                         "thread 45 '': xcl.exe+0x2 < game.exe+0x3"),
            *thread_dump(11, "thread 21 '': xcl.exe+0x11 < game.exe+0x12"),
            activity(12, 0),
        ]
        report = summarize(lines, title_exe="Game.exe")
        self.assertIn("Thread dump at 12.000 s: 4/4 thread lines parsed, 3 with a visible title frame", report)
        self.assertIn("Sampled title-thread candidates: 2", report)
        self.assertIn("Thread 42: 0xa < 0xb\n", report)
        self.assertTrue(report.endswith("Thread 45: 0x2"))
        self.assertNotIn("Thread 21", report)
        self.assertEqual(summarize(lines), summarize([activity(8, 50), activity(10, 0), activity(12, 0)]))

    def test_ignores_dumps_at_opening_sample_outside_window_and_after_shutdown(self):
        report = summarize([
            activity(8, 50),
            *thread_dump(8),
            activity(10, 0),
            *thread_dump(10.5),
            log(11, "XCL_RUN_SECONDS: 11 s – ending the title (50 frames shown, 50 PresentX calls)"),
            *thread_dump(11.5),
            activity(12, 0),
        ], title_exe="game.exe")
        self.assertIn("No timestamped thread dump in the sampled window.", report)

    def test_latest_partial_dump_reports_coverage(self):
        report = summarize([
            activity(8, 50),
            *thread_dump(9),
            *thread_dump(10, "thread 60 '': game.exe+0x10", count=2),
            activity(10, 0),
            "thread 61 '': xcl.exe+0x62 < game.exe+0x63\n",
        ], title_exe="game.exe")
        self.assertIn("1/2 thread lines parsed, 1 with a visible title frame (incomplete dump)", report)
        self.assertIn("Sampled title-thread candidates: 0", report)

    def test_zero_thread_dump_is_a_valid_sample(self):
        report = summarize([activity(8, 50), *thread_dump(9), activity(10, 0)], title_exe="game.exe")
        self.assertIn("Thread dump at 9.000 s: 0/0 thread lines parsed, 0 with a visible title frame", report)
        self.assertTrue(report.endswith("Sampled title-thread candidates: 0"))

    def test_unterminated_last_thread_row_is_incomplete(self):
        report = summarize([
            activity(8, 50),
            activity(10, 0),
            log(10, "thread dump (1 threads):"),
            "thread 61 '': xcl.exe+0x1 < game.exe+0x2",
        ], title_exe="game.exe")
        self.assertIn("0/1 thread lines parsed, 0 with a visible title frame (incomplete dump)", report)
        self.assertTrue(report.endswith("Sampled title-thread candidates: 0"))


if __name__ == "__main__":
    unittest.main()
