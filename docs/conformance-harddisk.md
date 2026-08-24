## Adapter conformance

Measured against `IBM      WDS-L80          SA00  disk`.

A refusal from the drive is not an adapter limitation: it means the command was 
carried there and the answer carried back. Only the middle table lists things no 
attached device can change.

### Supported

| command | |
|---|---|
| `0x00 TEST UNIT READY` | executed |
| `0x01 REZERO UNIT` | executed |
| `0x03 REQUEST SENSE` | executed |
| `0x08 READ(6)` | executed |
| `0x0b SEEK(6)` | executed |
| `0x12 INQUIRY` | executed |
| `0x16 RESERVE(6)` | executed |
| `0x17 RELEASE(6)` | executed |
| `0x1a MODE SENSE(6)` | executed |
| `0x1b START STOP UNIT (start)` | executed |
| `0x25 READ CAPACITY(10)` | executed |
| `0x28 READ(10)` | executed |
| `0x2b SEEK(10)` | executed |
| `0x2f VERIFY(10)` | executed |
| `0x37 READ DEFECT DATA(10)` | executed |
| `0x3c READ BUFFER` | executed |
| `INQUIRY alloc 0` | executed |
| `INQUIRY alloc 1` | executed |
| `INQUIRY alloc 4` | executed |
| `INQUIRY alloc 5` | executed |
| `INQUIRY alloc 36` | executed |
| `INQUIRY alloc 96` | executed |
| `INQUIRY alloc 255` | executed |

### Not supported, and cannot be

| limit | why |
|---|---|
| targets above 7 | the bus is narrow, and nothing in the protocol carries a high byte |
| devices behind a SCSI expander | they need a target ID of their own, and there are only eight |
| transfers of 64 KB or more | the firmware's internal buffer is smaller than that |

### May work with other devices

| command | origin | this drive |
|---|---|---|
| `0x1c RECEIVE DIAGNOSTIC RESULTS` | SCSI-2 optional | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x1e PREVENT ALLOW MEDIUM REMOVAL (allow)` | SCSI-2 optional | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x34 PRE-FETCH(10)` | SCSI-2 optional | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x35 SYNCHRONIZE CACHE(10)` | SCSI-2 optional | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x4d LOG SENSE` | SCSI-2 optional | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x5a MODE SENSE(10)` | SCSI-2 optional | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0xa8 READ(12)` | SCSI-2 other type | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x88 READ(16)` | SCSI-3 | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x9e READ CAPACITY(16)` | SCSI-3 | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0xa0 REPORT LUNS` | SCSI-3 | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
| `0x28 READ(10) past end of medium` | SCSI-2 mandatory | refused: illegal request (key 0x05, asc 0x21, ascq 0x00) |
| `0xff vendor-reserved opcode 0xFF` | SCSI-2 optional | refused: invalid opcode (illegal request (key 0x05, asc 0x20, ascq 0x00)) |
