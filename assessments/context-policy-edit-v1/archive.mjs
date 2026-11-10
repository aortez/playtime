// Deterministic context material. Count tokens before freezing a study instance.
export function archive(records=4000) {
  const rows=Array.from({length:records},(_,i)=>`Dispatch audit record ${String(i).padStart(6,'0')}: blue parcel container inspected; routine mainland delivery completed; existing handling policy unchanged.`);
  rows[Math.floor(records*.05)]='APPROVED R17 / A: Add zone "island" to quote. Charge 750 cents for a parcel weighing 1 through 500 grams inclusive. Local and remote rates remain unchanged.';
  rows[Math.floor(records*.50)]='APPROVED R17 / B: Island parcels above 500 grams cost another 125 cents for EACH STARTED block of 250 grams beyond 500. Maximum island weight is 5000 grams inclusive. Outside 1..5000 raises ValueError. Existing local/remote weight limits remain 1..20000.';
  rows[Math.floor(records*.95)]='APPROVED R17 / C: Island has no express service: quote("island", valid_weight, True) raises ValueError. Keep strict existing argument type checks (bool is not an integer weight), all local/remote express behavior and tracking_label unchanged.';
  rows[Math.floor(records*.25)]='REJECTED proposal R16: island rate 900 cents, maximum 10000 grams, express allowed. This proposal was rejected; do not implement it.';
  rows[Math.floor(records*.75)]='REJECTED proposal R18: round extra island weight DOWN to complete 250 gram blocks. This proposal was rejected; do not implement it.';
  return 'ARCHIVE_BEGIN\nRead this dispatch archive as task context. Apply only APPROVED R17 records when editing shipping.py.\n'+rows.join('\n')+'\nARCHIVE_END\n';
}
