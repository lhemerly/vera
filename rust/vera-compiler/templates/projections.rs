#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProjectionValue { Bool(bool), Integer(I256), Bytes32([u8;32]), Bytes(Vec<u8>), Record { ty: &'static str, fields: Vec<(&'static str,ProjectionValue)> }, Variant { ty: &'static str, tag: &'static str, payload: Box<ProjectionValue> } }
impl ProjectionValue {fn offset(self,n:i128)->Option<Self>{match self{Self::Integer(v)=>v.checked_add(I256::from_i128(n)).map(Self::Integer),v if n==0=>Some(v),_=>None}}}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionCell {pub table:&'static str,pub column:&'static str,pub requested:Option<u64>,pub id:u64,pub live:bool,pub value:Option<ProjectionValue>}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParameterBound {pub parameter:&'static str,pub min:Option<ProjectionValue>,pub max:Option<ProjectionValue>,pub exact:Option<ProjectionValue>}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Affordance {pub transition:&'static str,pub enabled:bool,pub bounds:Vec<ParameterBound>}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionFrame {pub projection:&'static str,pub allowed:bool,pub reason:&'static str,pub cells:Vec<ProjectionCell>,pub affordances:Vec<Affordance>}
fn projection_bound(bounds:&mut[ParameterBound],parameter:&str,kind:&str,value:ProjectionValue){if let Some(b)=bounds.iter_mut().find(|b|b.parameter==parameter){match kind{"min"=>if b.min.as_ref().map_or(true,|v|*v<value){b.min=Some(value)},"max"=>if b.max.as_ref().map_or(true,|v|*v>value){b.max=Some(value)},"exact"=>b.exact=Some(value),_=>{}}}}
