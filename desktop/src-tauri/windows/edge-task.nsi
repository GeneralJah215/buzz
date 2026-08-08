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
;     privilege problem.
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
; NOTE: an orphaned logon task that launches a deleted binary is a bad thing
; to leave behind, so the uninstall hooks delete the task unconditionally -
; including when the task was never registered, where the failure is ignored
; on purpose.
;
; The task definition below MUST stay in lockstep with
; src/edge_supervisor/host.rs::task_definition_xml. The supervisor repairs a
; missing or outdated registration at every Desktop launch using that
; function; if the two drift, the supervisor "repairs" a task the installer
; just registered, on every single launch. A Rust test asserts that the task
; name, the quoting, the run level, and the restart policy match.

!define BUZZ_EDGE_TASK_NAME "Buzz Edge Sidecar"
!define BUZZ_EDGE_EXE "buzz-edge.exe"
!define BUZZ_EDGE_TASK_XML "$PLUGINSDIR\buzz-edge-task.xml"

; Stop a running sidecar so its binary can be replaced or removed.
; Best effort: nothing here fails the install if the process is not running.
!macro BuzzEdgeStopSidecar
  nsExec::Exec '"$SYSDIR\schtasks.exe" /End /TN "${BUZZ_EDGE_TASK_NAME}"'
  Pop $0
  nsExec::Exec '"$SYSDIR\taskkill.exe" /IM "${BUZZ_EDGE_EXE}" /F'
  Pop $0
!macroend

; Remove the task. Runs on uninstall, and is also safe to call when no task
; exists - the non-zero exit code is discarded deliberately.
!macro BuzzEdgeDeleteTask
  nsExec::Exec '"$SYSDIR\schtasks.exe" /Delete /TN "${BUZZ_EDGE_TASK_NAME}" /F'
  Pop $0
!macroend

; Write the Task Scheduler definition. Task Scheduler requires UTF-16 for
; /XML input, which is why FileOpen uses the "w" mode with a BOM written by
; FileWriteUTF16LE.
!macro BuzzEdgeWriteTaskXml
  InitPluginsDir
  FileOpen $9 "${BUZZ_EDGE_TASK_XML}" w
  FileWriteUTF16LE /BOM $9 '<?xml version="1.0" encoding="UTF-16"?>$\r$\n'
  FileWriteUTF16LE $9 '<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">$\r$\n'
  FileWriteUTF16LE $9 '  <RegistrationInfo>$\r$\n'
  FileWriteUTF16LE $9 '    <Description>Starts the Buzz edge sidecar at logon (app version ${VERSION}).</Description>$\r$\n'
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
  FileWriteUTF16LE $9 '      <Command>"$INSTDIR\${BUZZ_EDGE_EXE}"</Command>$\r$\n'
  FileWriteUTF16LE $9 '      <WorkingDirectory>$INSTDIR</WorkingDirectory>$\r$\n'
  FileWriteUTF16LE $9 '    </Exec>$\r$\n'
  FileWriteUTF16LE $9 '  </Actions>$\r$\n'
  FileWriteUTF16LE $9 '</Task>$\r$\n'
  FileClose $9
!macroend

; ── Tauri hooks ──────────────────────────────────────────────────────────

; Before files are written: on an upgrade the old sidecar still holds its own
; binary open, and Windows will not overwrite a running image.
!macro NSIS_HOOK_PREINSTALL
  !insertmacro BuzzEdgeStopSidecar
!macroend

; After files are written: register (or re-register, via /F) the task.
;
; The registration is skipped when the sidecar binary is not present. The
; sidecar ships as a Tauri externalBin; until that entry exists, a build
; produces an installer with no buzz-edge.exe, and registering a logon task
; that points at a missing file would fail on every logon forever.
!macro NSIS_HOOK_POSTINSTALL
  IfFileExists "$INSTDIR\${BUZZ_EDGE_EXE}" 0 buzz_edge_skip_register
    !insertmacro BuzzEdgeWriteTaskXml
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
  buzz_edge_skip_register:
    DetailPrint "Buzz: no edge sidecar in this build; skipping scheduled-task registration."
    ; An older install may have registered a task for a binary this build no
    ; longer ships. Leaving it would start a deleted binary at every logon.
    !insertmacro BuzzEdgeDeleteTask
  buzz_edge_register_done:
!macroend

; Before the uninstaller removes files: stop the sidecar so its binary is not
; locked, and drop the task first so no logon can re-launch it mid-uninstall.
!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro BuzzEdgeStopSidecar
  !insertmacro BuzzEdgeDeleteTask
!macroend

; Belt and braces: if the pre-uninstall hook was skipped for any reason, the
; task must still not survive the uninstall.
!macro NSIS_HOOK_POSTUNINSTALL
  !insertmacro BuzzEdgeDeleteTask
!macroend
