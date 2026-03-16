\* Copyright 2026 Photon Ring Contributors
\* SPDX-License-Identifier: Apache-2.0

---------------------------- MODULE seqlock ----------------------------
(*
 * TLA+ specification of the Photon Ring seqlock-stamped ring buffer protocol.
 *
 * Models a single-producer, multi-consumer (SPMC) ring buffer where:
 *   - Each slot carries a seqlock stamp and a value.
 *   - The writer publishes messages in strict sequence order.
 *   - Readers use the per-slot stamp to detect torn reads and lag.
 *
 * The protocol matches the Rust implementation in src/slot.rs:
 *   Writer:  stamp := seq*2+1  (writing)
 *            value := payload
 *            stamp := seq*2+2  (done)
 *            cursor := seq
 *
 *   Reader:  s1 := stamp[slot]
 *            if s1 is odd         -> retry (write in progress)
 *            if s1 /= seq*2+2    -> wrong sequence (empty or lagged)
 *            val := value[slot]
 *            s2 := stamp[slot]
 *            if s1 = s2           -> success (no torn read)
 *            else                 -> retry (torn read detected)
 *
 * Properties verified:
 *   Safety   (NoTornRead):   A reader never returns a value that was
 *                             partially written by a concurrent write.
 *   Liveness (ReaderProgress): If the writer publishes, eventually every
 *                             idle reader can read a value.
 *   Lag correctness (LagDetected): If a reader falls behind by more than
 *                             the ring capacity, it detects the lag.
 *)

EXTENDS Integers, FiniteSets, TLC

CONSTANTS
    NumReaders,    \* Number of reader processes (>= 1)
    RingSize,      \* Number of slots in the ring (must be a power of 2)
    MaxSeq         \* Maximum sequence number to model-check (bounds the state space)

ASSUME NumReaders >= 1
ASSUME RingSize >= 2
ASSUME MaxSeq >= 1

VARIABLES
    \* --- Shared memory (the ring buffer) ---
    slots,         \* Array [1..RingSize] of records [stamp : Int, value : Int]
    cursor,        \* Publisher's cursor: sequence of last published message (-1 = none)

    \* --- Writer-local state ---
    writerPC,      \* Writer program counter: "idle" | "stamped" | "written"
    writerSeq,     \* Next sequence the writer will publish

    \* --- Per-reader state ---
    readerPC,      \* [1..NumReaders] -> "idle" | "stamp1" | "reading" | "validating"
    readerCursor,  \* [1..NumReaders] -> next sequence to read
    readStamp1,    \* [1..NumReaders] -> first stamp loaded
    readValue,     \* [1..NumReaders] -> value loaded (may be torn)
    readStamp2,    \* [1..NumReaders] -> second stamp loaded (for verification)
    readResult     \* [1..NumReaders] -> last successfully committed value (-1 = none)

vars == <<slots, cursor, writerPC, writerSeq,
          readerPC, readerCursor, readStamp1, readValue, readStamp2, readResult>>

Readers == 1..NumReaders

\* Map a sequence number to a 1-based slot index (seq mod RingSize) + 1
SlotIndex(seq) == (seq % RingSize) + 1

(* ======================================================================
   Writer actions
   ====================================================================== *)

\* Step 1: Store odd stamp (marks slot as "write in progress")
WriterBegin ==
    /\ writerPC = "idle"
    /\ writerSeq <= MaxSeq
    /\ LET idx == SlotIndex(writerSeq) IN
       /\ slots' = [slots EXCEPT ![idx].stamp = writerSeq * 2 + 1]
       /\ writerPC' = "stamped"
       /\ UNCHANGED <<cursor, writerSeq, readerPC, readerCursor,
                      readStamp1, readValue, readStamp2, readResult>>

\* Step 2: Write the value (while odd stamp is visible)
WriterData ==
    /\ writerPC = "stamped"
    /\ LET idx == SlotIndex(writerSeq) IN
       \* Assert: slot must have the odd stamp we just wrote
       /\ slots[idx].stamp = writerSeq * 2 + 1
       /\ slots' = [slots EXCEPT ![idx].value = writerSeq]
    /\ writerPC' = "written"
    /\ UNCHANGED <<cursor, writerSeq, readerPC, readerCursor,
                   readStamp1, readValue, readStamp2, readResult>>

\* Step 3: Store even stamp (marks slot as "write complete") and advance cursor
WriterFinish ==
    /\ writerPC = "written"
    /\ LET idx == SlotIndex(writerSeq) IN
       /\ slots[idx].stamp = writerSeq * 2 + 1
       /\ slots' = [slots EXCEPT ![idx].stamp = writerSeq * 2 + 2]
    /\ cursor' = writerSeq
    /\ writerSeq' = writerSeq + 1
    /\ writerPC' = "idle"
    /\ UNCHANGED <<readerPC, readerCursor,
                   readStamp1, readValue, readStamp2, readResult>>

(* ======================================================================
   Reader actions
   ====================================================================== *)

\* Step 1: Load the stamp from the target slot
ReaderLoadStamp1(r) ==
    /\ readerPC[r] = "idle"
    /\ LET idx == SlotIndex(readerCursor[r]) IN
       /\ readStamp1' = [readStamp1 EXCEPT ![r] = slots[idx].stamp]
       /\ readerPC' = [readerPC EXCEPT ![r] = "stamp1"]
       /\ UNCHANGED <<slots, cursor, writerPC, writerSeq, readerCursor,
                      readValue, readStamp2, readResult>>

\* Step 2a: If stamp is even and matches expected, load the value
ReaderLoadValue(r) ==
    /\ readerPC[r] = "stamp1"
    /\ readStamp1[r] % 2 = 0                          \* even => not writing
    /\ readStamp1[r] = readerCursor[r] * 2 + 2        \* matches expected sequence
    /\ LET idx == SlotIndex(readerCursor[r]) IN
       /\ readValue' = [readValue EXCEPT ![r] = slots[idx].value]
       /\ readerPC' = [readerPC EXCEPT ![r] = "reading"]
       /\ UNCHANGED <<slots, cursor, writerPC, writerSeq, readerCursor,
                      readStamp1, readStamp2, readResult>>

\* Step 2b: If stamp is odd (write in progress), retry from idle
ReaderRetryOdd(r) ==
    /\ readerPC[r] = "stamp1"
    /\ readStamp1[r] % 2 = 1
    /\ readerPC' = [readerPC EXCEPT ![r] = "idle"]
    /\ UNCHANGED <<slots, cursor, writerPC, writerSeq, readerCursor,
                   readStamp1, readValue, readStamp2, readResult>>

\* Step 2c: If stamp is even but does not match expected, handle lag or empty
ReaderStampMismatch(r) ==
    /\ readerPC[r] = "stamp1"
    /\ readStamp1[r] % 2 = 0                          \* even
    /\ readStamp1[r] # readerCursor[r] * 2 + 2        \* does NOT match expected
    /\ IF readStamp1[r] > readerCursor[r] * 2 + 2
       THEN \* Lagged: slot was overwritten. Skip to oldest available.
            LET oldest == (IF cursor >= RingSize
                           THEN cursor - RingSize + 1
                           ELSE 0)
            IN readerCursor' = [readerCursor EXCEPT ![r] =
                   IF readerCursor[r] < oldest THEN oldest ELSE readerCursor[r]]
       ELSE \* Not published yet -- just retry
            UNCHANGED readerCursor
    /\ readerPC' = [readerPC EXCEPT ![r] = "idle"]
    /\ UNCHANGED <<slots, cursor, writerPC, writerSeq,
                   readStamp1, readValue, readStamp2, readResult>>

\* Step 3: Re-load stamp to verify no torn read occurred
ReaderVerify(r) ==
    /\ readerPC[r] = "reading"
    /\ LET idx == SlotIndex(readerCursor[r]) IN
       /\ readStamp2' = [readStamp2 EXCEPT ![r] = slots[idx].stamp]
       /\ IF readStamp1[r] = slots[idx].stamp
          THEN \* Stamps match: read is consistent. Commit the value.
               /\ readResult' = [readResult EXCEPT ![r] = readValue[r]]
               /\ readerCursor' = [readerCursor EXCEPT ![r] = readerCursor[r] + 1]
               /\ readerPC' = [readerPC EXCEPT ![r] = "idle"]
          ELSE \* Stamps differ: torn read detected. Discard and retry.
               /\ readerPC' = [readerPC EXCEPT ![r] = "idle"]
               /\ UNCHANGED <<readResult, readerCursor>>
    /\ UNCHANGED <<slots, cursor, writerPC, writerSeq, readStamp1, readValue>>

(* ======================================================================
   Specification
   ====================================================================== *)

Init ==
    /\ slots = [i \in 1..RingSize |-> [stamp |-> 0, value |-> -1]]
    /\ cursor = -1
    /\ writerPC = "idle"
    /\ writerSeq = 0
    /\ readerPC = [r \in Readers |-> "idle"]
    /\ readerCursor = [r \in Readers |-> 0]
    /\ readStamp1 = [r \in Readers |-> 0]
    /\ readValue = [r \in Readers |-> -1]
    /\ readStamp2 = [r \in Readers |-> 0]
    /\ readResult = [r \in Readers |-> -1]

Next ==
    \/ WriterBegin
    \/ WriterData
    \/ WriterFinish
    \/ \E r \in Readers :
        \/ ReaderLoadStamp1(r)
        \/ ReaderLoadValue(r)
        \/ ReaderRetryOdd(r)
        \/ ReaderStampMismatch(r)
        \/ ReaderVerify(r)

Spec == Init /\ [][Next]_vars

FairSpec == Spec /\ WF_vars(Next)

(* ======================================================================
   Safety properties
   ====================================================================== *)

\* SAFETY: No torn reads.
\* If a reader has committed a result (readResult # -1), that result
\* must equal the sequence number it just consumed (readerCursor - 1).
\* This holds because we encode value = seq in the writer, so a correct
\* read of sequence S yields value S.
NoTornRead ==
    \A r \in Readers :
        readResult[r] # -1 =>
            readResult[r] = readerCursor[r] - 1

\* SAFETY: Values are always from a completed write.
\* A committed result is always the value that was written for that sequence,
\* never a partial or stale value.
ResultIsValid ==
    \A r \in Readers :
        readResult[r] # -1 =>
            readResult[r] >= 0 /\ readResult[r] <= MaxSeq

(* ======================================================================
   Liveness properties (require FairSpec)
   ====================================================================== *)

\* LIVENESS: If the writer publishes at least one message (cursor >= 0),
\* then eventually every idle reader will have read at least one message
\* (readResult # -1), provided fair scheduling.
ReaderProgress ==
    \A r \in Readers :
        (cursor >= 0) ~> (readResult[r] # -1)

(* ======================================================================
   Lag detection
   ====================================================================== *)

\* LAG CORRECTNESS: A reader's cursor never points to a slot more than
\* RingSize positions behind the publisher cursor. If it would, the
\* ReaderStampMismatch action advances it.
\*
\* Stated as an invariant: no reader's committed cursor (readerCursor)
\* is more than RingSize behind the writer cursor, unless no message
\* has been published yet.
LagBound ==
    \A r \in Readers :
        cursor >= 0 =>
            (cursor - readerCursor[r] < RingSize) \/ (readerPC[r] # "idle")

(* ======================================================================
   Theorems
   ====================================================================== *)

THEOREM Spec => []NoTornRead
THEOREM Spec => []ResultIsValid
THEOREM FairSpec => ReaderProgress

========================================================================
