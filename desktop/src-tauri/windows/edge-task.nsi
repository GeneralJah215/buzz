; Buzz edge sidecar - Scheduled Task installer hooks
; SPEC-2026-08-05 "Buzz Edge phase 1", packaging note + acceptance item 17.
;
; Wired from tauri.conf.json via bundle > windows > nsis > installerHooks.
; Tauri includes this file inside its generated installer script and invokes
; the four NSIS_HOOK_* macros below.
;
; What it does
;   install    register a logon Scheduled Task that starts buzz-edge.exe
;   upgrade    stop the running sidecar, then re-register the task (/F) so it
;              points at the freshly written binary
;   uninstall  stop the sidecar and delete the task
;
; Acceptance item 17 requires a QUOTED path and USER scope:
;   * The <Command> element below is quoted. This path contains
;     "Program Files"; an unquoted C:\Program Files\Buzz\buzz-edge.exe is read
;     by Windows as C:\Program.exe with arguments - the classic unquoted-path
;     privilege problem. The quotes are written as &quot;, because <Command> is
;     XML: &quot; and " are the same character to any XML parser.
;   * <RunLevel>LeastPrivilege</RunLevel> on an InteractiveToken principal
;     means the task runs with the logged-on user's limited token. No
;     elevation, ever. No /RU and no /RP are passed, so schtasks binds the
;     task to the account running the installer.
;
; ASSUMPTION, stated because nothing here enforces it: the NSIS bundle uses
; Tauri's default per-user install mode. Under a machine-wide elevated
; install the "installing user" is the elevating administrator, and the task
; would be registered for that account instead. If installMode is ever set to
; "perMachine", this hook must be revisited.
;
; XML ESCAPING. $INSTDIR is not a safe XML value. With Tauri's default
; currentUser install mode it sits under %LOCALAPPDATA%, which contains the
; Windows user name, and "&" is a perfectly legal character in a Windows user
; name. A user called "Tom & Jerry" interpolated raw produces malformed XML,
; schtasks /Create /XML fails, and under /S the only signal is a DetailPrint
; nobody sees. So every interpolated value goes through BuzzEdgeXmlEscape
; first. host.rs::escape_xml_text does the same on the supervisor side, and
; host.rs::unescape_xml_text undoes it when the definition is read back - an
; escaped write with a raw read would turn the hard failure into a permanent
; "repair" of a task that is already correct.
;
; REGISTERS. NSIS registers are global, and this file is spliced into Tauri's
; generated script, which uses $0-$9 and $R0-$R9 itself. Every hook macro
; below saves and restores every register it touches. The inner macros do not
; save anything; they document which registers they use and rely on the hook.
;
; The task definition below MUST stay in lockstep with
; src/edge_supervisor/host.rs::task_definition_xml. The supervisor repairs a
; missing or outdated registration at every Desktop launch using that
; function; if the two drift, the supervisor "repairs" a task the installer
; just registered, on every single launch. A Rust test rebuilds the XML from
; the FileWriteUTF16LE lines below and compares it, line for line, against
; task_definition_xml's actual output.
;
; NOT VERIFIED HERE (see the S-list in src/edge_supervisor.rs): whether Task
; Scheduler preserves the quotes inside <Command>, whether <RestartOnFailure>
; is accepted in this position in the Settings sequence, and whether taskkill's
; USERNAME filter matches a bare user name. Settling any of them needs one
; registration on a real machine.

!define BUZZ_EDGE_TASK_NAME "Buzz Edge Sidecar"
!define BUZZ_EDGE_EXE "buzz-edge.exe"
!define BUZZ_EDGE_TASK_XML "$PLUGINSDIR\buzz-edge-task.xml"
; How long the sidecar gets to close its SQLite database and drain its outbox
; after being asked politely, before it is forced.
!define BUZZ_EDGE_STOP_GRACE_MS 3000
; How long to wait after the kill before files are removed, so the process has
; released its handles.
!define BUZZ_EDGE_SETTLE_MS 1000

; Escape &, < , > and " for XML element content.
; Uses $R1 and $R2 as scratch; the caller saves them. ${OUT} and ${IN} must be
; different registers. ${UID} makes the labels unique, because a macro inserted
; twice inside one section would otherwise define the same label twice.
; "&" is replaced first, or the replacements' own ampersands get escaped again.
!macro BuzzEdgeXmlEscape OUT IN UID
  StrCpy ${OUT} ""
  StrCpy $R1 0
  buzz_edge_esc_${UID}:
    StrCpy $R2 "${IN}" 1 $R1
    StrCmp $R2 "" buzz_edge_esc_done_${UID}
    IntOp $R1 $R1 + 1
    StrCmp $R2 "&" 0 buzz_edge_esc_lt_${UID}
      StrCpy ${OUT} "${OUT}&amp;"
      Goto buzz_edge_esc_${UID}
    buzz_edge_esc_lt_${UID}:
    StrCmp $R2 "<" 0 buzz_edge_esc_gt_${UID}
      StrCpy ${OUT} "${OUT}&lt;"
      Goto buzz_edge_esc_${UID}
    buzz_edge_esc_gt_${UID}:
    StrCmp $R2 ">" 0 buzz_edge_esc_quot_${UID}
      StrCpy ${OUT} "${OUT}&gt;"
      Goto buzz_edge_esc_${UID}
    buzz_edge_esc_quot_${UID}:
    StrCmp $R2 '"' 0 buzz_edge_esc_raw_${UID}
      StrCpy ${OUT} "${OUT}&quot;"
      Goto buzz_edge_esc_${UID}
    buzz_edge_esc_raw_${UID}:
    StrCpy ${OUT} "${OUT}$R2"
    Goto buzz_edge_esc_${UID}
  buzz_edge_esc_done_${UID}:
!macroend

; Stop a running sidecar so its binary can be replaced or removed.
; Uses $0 and $R0; the caller saves them.
;
; Three things this deliberately does NOT do, each of which the previous
; version did:
;   * It does not run a bare `taskkill /IM buzz-edge.exe /F`. That has no user
;     filter, so under an elevated uninstall it kills every logged-on user's
;     sidecar, plus any unrelated process that happens to share the image name.
;   * It does not force-kill first. The sidecar holds an open SQLite database
;     and a live outbox; /F with no warning is a torn write waiting to happen.
;   * It does not go straight on to deleting files. A killed process needs a
;     moment to release its handles.
;
; Order: ask Task Scheduler to end the instance it started (scoped and graceful
; by construction, and the normal case), then ask the process to close, wait,
; then force, then settle.
!macro BuzzEdgeStopSidecar UID
  nsExec::Exec '"$SYSDIR\schtasks.exe" /End /TN "${BUZZ_EDGE_TASK_NAME}"'
  Pop $0
  ReadEnvStr $R0 "USERNAME"
  StrCmp $R0 "" buzz_edge_stop_unscoped_${UID}
  nsExec::Exec '"$SYSDIR\taskkill.exe" /IM "${BUZZ_EDGE_EXE}" /T /FI "USERNAME eq $R0"'
  Pop $0
  ; Non-zero means nothing matched the filter, so there is nothing to wait for.
  StrCmp $0 "0" 0 buzz_edge_stop_done_${UID}
  Sleep ${BUZZ_EDGE_STOP_GRACE_MS}
  nsExec::Exec '"$SYSDIR\taskkill.exe" /IM "${BUZZ_EDGE_EXE}" /T /F /FI "USERNAME eq $R0"'
  Pop $0
  Sleep ${BUZZ_EDGE_SETTLE_MS}
  Goto buzz_edge_stop_done_${UID}
  buzz_edge_stop_unscoped_${UID}:
  ; No USERNAME in the environment is not a licence to kill every session's
  ; sidecar. Say so and stop; the /End above already covered the instance Task
  ; Scheduler started, which is the one this installer is responsible for.
  DetailPrint "Buzz: USERNAME is not set; skipping the sidecar taskkill rather than killing every session's process."
  buzz_edge_stop_done_${UID}:
!macroend

; Remove the task. Runs on uninstall, and is also safe to call when no task
; exists - the non-zero exit code is discarded deliberately.
; Uses $0; the caller saves it.
!macro BuzzEdgeDeleteTask
  nsExec::Exec '"$SYSDIR\schtasks.exe" /Delete /TN "${BUZZ_EDGE_TASK_NAME}" /F'
  Pop $0
!macroend

; Write the Task Scheduler definition. Task Scheduler requires UTF-16 for
; /XML input, which is why FileOpen uses the "w" mode with a BOM written by
; FileWriteUTF16LE.
;
; Returns "1" in $R5 when the file was written and "0" when it was not. A
; failed FileOpen leaves an empty handle and every FileWriteUTF16LE then
; silently no-ops, which the previous version went on to report as a schtasks
; exit code - sending whoever read the log to entirely the wrong place.
;
; Uses $R1, $R2, $R5, $R6, $R7, $R8 and $9; the caller saves them.
!macro BuzzEdgeWriteTaskXml
  InitPluginsDir
  StrCpy $R5 "0"
  !insertmacro BuzzEdgeXmlEscape $R8 "$INSTDIR\${BUZZ_EDGE_EXE}" cmd
  !insertmacro BuzzEdgeXmlEscape $R7 "$INSTDIR" dir
  !insertmacro BuzzEdgeXmlEscape $R6 "${VERSION}" ver
  FileOpen $9 "${BUZZ_EDGE_TASK_XML}" w
  StrCmp $9 "" buzz_edge_xml_unwritable
  FileWriteUTF16LE /BOM $9 '<?xml version="1.0" encoding="UTF-16"?>$\r$\n'
  FileWriteUTF16LE $9 '<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">$\r$\n'
  FileWriteUTF16LE $9 '  <RegistrationInfo>$\r$\n'
  FileWriteUTF16LE $9 '    <Description>Starts the Buzz edge sidecar at logon (app version $R6).</Description>$\r$\n'
  FileWriteUTF16LE $9 '  </RegistrationInfo>$\r$\n'
  FileWriteUTF16LE $9 '  <Triggers>$\r$\n'
  FileWriteUTF16LE $9 '    <LogonTrigger>$\r$\n'
  FileWriteUTF16LE $9 '      <Enabled>true</Enabled>$\r$\n'
  FileWriteUTF16LE $9 '    </LogonTrigger>$\r$\n'
  FileWriteUTF16LE $9 '  </Triggers>$\r$\n'
  FileWriteUTF16LE $9 '  <Principals>$\r$\n'
  FileWriteUTF16LE $9 '    <Principal id="Author">$\r$\n'
  FileWriteUTF16LE $9 '      <LogonType>InteractiveToken</LogonType>$\r$\n'
  FileWriteUTF16LE $9 '      <RunLevel>LeastPrivilege</RunLevel>$\r$\n'
  FileWriteUTF16LE $9 '    </Principal>$\r$\n'
  FileWriteUTF16LE $9 '  </Principals>$\r$\n'
  FileWriteUTF16LE $9 '  <Settings>$\r$\n'
  FileWriteUTF16LE $9 '    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>$\r$\n'
  FileWriteUTF16LE $9 '    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>$\r$\n'
  FileWriteUTF16LE $9 '    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>$\r$\n'
  FileWriteUTF16LE $9 '    <AllowHardTerminate>true</AllowHardTerminate>$\r$\n'
  FileWriteUTF16LE $9 '    <StartWhenAvailable>false</StartWhenAvailable>$\r$\n'
  FileWriteUTF16LE $9 '    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>$\r$\n'
  FileWriteUTF16LE $9 '    <RestartOnFailure>$\r$\n'
  FileWriteUTF16LE $9 '      <Interval>PT1M</Interval>$\r$\n'
  FileWriteUTF16LE $9 '      <Count>3</Count>$\r$\n'
  FileWriteUTF16LE $9 '    </RestartOnFailure>$\r$\n'
  FileWriteUTF16LE $9 '    <AllowStartOnDemand>true</AllowStartOnDemand>$\r$\n'
  FileWriteUTF16LE $9 '    <Enabled>true</Enabled>$\r$\n'
  FileWriteUTF16LE $9 '    <Hidden>false</Hidden>$\r$\n'
  FileWriteUTF16LE $9 '    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>$\r$\n'
  FileWriteUTF16LE $9 '    <Priority>7</Priority>$\r$\n'
  FileWriteUTF16LE $9 '  </Settings>$\r$\n'
  FileWriteUTF16LE $9 '  <Actions Context="Author">$\r$\n'
  FileWriteUTF16LE $9 '    <Exec>$\r$\n'
  FileWriteUTF16LE $9 '      <Command>&quot;$R8&quot;</Command>$\r$\n'
  FileWriteUTF16LE $9 '      <WorkingDirectory>$R7</WorkingDirectory>$\r$\n'
  FileWriteUTF16LE $9 '    </Exec>$\r$\n'
  FileWriteUTF16LE $9 '  </Actions>$\r$\n'
  FileWriteUTF16LE $9 '</Task>$\r$\n'
  FileClose $9
  StrCpy $R5 "1"
  buzz_edge_xml_unwritable:
!macroend

; ── Tauri hooks ──────────────────────────────────────────────────────────

; Before files are written: on an upgrade the old sidecar still holds its own
; binary open, and Windows will not overwrite a running image.
!macro NSIS_HOOK_PREINSTALL
  Push $0
  Push $R0
  !insertmacro BuzzEdgeStopSidecar preinstall
  Pop $R0
  Pop $0
!macroend

; After files are written: register (or re-register, via /F) the task.
;
; The registration is skipped when the sidecar binary is not present. The
; sidecar ships as a Tauri externalBin; if that entry is ever removed, a build
; produces an installer with no buzz-edge.exe, and registering a logon task
; that points at a missing file would fail on every logon forever.
; edge_supervisor::decide_task_repair applies the same rule at launch, so the
; supervisor cannot create the orphan this branch refuses to create.
!macro NSIS_HOOK_POSTINSTALL
  Push $0
  Push $9
  Push $R1
  Push $R2
  Push $R5
  Push $R6
  Push $R7
  Push $R8
  IfFileExists "$INSTDIR\${BUZZ_EDGE_EXE}" 0 buzz_edge_skip_register
    !insertmacro BuzzEdgeWriteTaskXml
    StrCmp $R5 "1" 0 buzz_edge_xml_failed
    nsExec::ExecToLog '"$SYSDIR\schtasks.exe" /Create /TN "${BUZZ_EDGE_TASK_NAME}" /XML "${BUZZ_EDGE_TASK_XML}" /F'
    Pop $0
    Delete "${BUZZ_EDGE_TASK_XML}"
    ; Plain StrCmp rather than LogicLib's ${If}: this file is included into
    ; Tauri's generated script, and depending on an .nsh that may or may not
    ; have been included yet would turn a hook into a build break.
    StrCmp $0 "0" buzz_edge_register_done 0
    ; Not fatal: Buzz runs canonical-only without the sidecar, and
    ; edge_supervisor.rs repairs a missing registration at the next launch
    ; (bounded to 3 attempts). Recorded in the installer log so the failure is
    ; visible rather than silent.
    DetailPrint "Buzz: could not register the edge sidecar task (schtasks exit $0); Buzz will retry at launch."
    Goto buzz_edge_register_done
  buzz_edge_xml_failed:
    ; Distinct from a schtasks failure on purpose. Reporting an unwritable temp
    ; file as a schtasks exit code sends the next reader to the wrong place.
    DetailPrint "Buzz: could not write the edge sidecar task definition to ${BUZZ_EDGE_TASK_XML}; skipping registration. Buzz will retry at launch."
    Goto buzz_edge_register_done
  buzz_edge_skip_register:
    DetailPrint "Buzz: no edge sidecar in this build; skipping scheduled-task registration."
    ; An older install may have registered a task for a binary this build no
    ; longer ships. Leaving it would start a deleted binary at every logon.
    !insertmacro BuzzEdgeDeleteTask
  buzz_edge_register_done:
  Pop $R8
  Pop $R7
  Pop $R6
  Pop $R5
  Pop $R2
  Pop $R1
  Pop $9
  Pop $0
!macroend

; Before the uninstaller removes files: stop the sidecar so its binary is not
; locked, and drop the task first so no logon can re-launch it mid-uninstall.
!macro NSIS_HOOK_PREUNINSTALL
  Push $0
  Push $R0
  !insertmacro BuzzEdgeStopSidecar preuninstall
  !insertmacro BuzzEdgeDeleteTask
  Pop $R0
  Pop $0
!macroend

; Belt and braces: if the pre-uninstall hook was skipped for any reason, the
; task must still not survive the uninstall - and neither must the process.
; Deleting the task while leaving the sidecar running was the previous
; version's gap: it described this hook as the fallback path and then did only
; half of what the fallback needs to do.
!macro NSIS_HOOK_POSTUNINSTALL
  Push $0
  Push $R0
  !insertmacro BuzzEdgeStopSidecar postuninstall
  !insertmacro BuzzEdgeDeleteTask
  Pop $R0
  Pop $0
!macroend
