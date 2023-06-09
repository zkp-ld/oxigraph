use crate::{
    bad_request, base_url, internal_server_error, query_results_content_negotiation, HttpError,
    ReadForWrite,
};
use oxhttp::model::{Request, Response};
use oxigraph::{sparql::QueryResults, store::Store};
use oxiri::Iri;
use sparesults::QueryResultsSerializer;
use spargebra::{
    algebra::{Expression, GraphPattern, QueryDataset},
    term::{GroundTerm, NamedNodePattern, TriplePattern, Variable},
};
use std::collections::HashSet;
use url::form_urlencoded;

pub(crate) fn configure_and_evaluate_zksparql_query(
    store: &Store,
    encoded: &[&[u8]],
    mut query: Option<String>,
    request: &Request,
) -> Result<Response, HttpError> {
    for encoded in encoded {
        for (k, v) in form_urlencoded::parse(encoded) {
            if let "query" = k.as_ref() {
                if query.is_some() {
                    return Err(bad_request("Multiple query parameters provided"));
                }
                query = Some(v.into_owned())
            }
        }
    }
    let query = query.ok_or_else(|| bad_request("You should set the 'query' parameter"))?;
    evaluate_zksparql_query(store, &query, request)
}

#[derive(Debug, Default)]
struct ZkQuery {
    disclosed_variables: Vec<Variable>,
    in_scope_variables: HashSet<Variable>,
    patterns: Vec<TriplePattern>,
    filter: Option<Expression>,
    values: Option<ZkQueryValues>,
    limit: Option<ZkQueryLimit>,
}

#[derive(Debug, Default)]
struct ZkQueryValues {
    variables: Vec<Variable>,
    bindings: Vec<Vec<Option<GroundTerm>>>,
}

#[derive(Debug, Default)]
struct ZkQueryLimit {
    start: usize,
    length: Option<usize>,
}

fn evaluate_zksparql_query(
    store: &Store,
    query: &str,
    request: &Request,
) -> Result<Response, HttpError> {
    // 1. parse a zk-SPARQL query
    let parsed_zk_query = parse_zk_query(query, request)?;
    println!("parsed_zk_query: {:#?}", parsed_zk_query);

    // 2. construct an extended query to identify credentials to be disclosed
    let extended_query = construct_extended_query(parsed_zk_query)?;
    println!("extended_query: {:#?}", extended_query);

    // 3. execute the extended query to get extended solutions
    let extended_results = store.query(extended_query).map_err(internal_server_error)?;

    // 4. generate VP if required

    // 5. return query results
    match extended_results {
        QueryResults::Solutions(solutions) => {
            let format = query_results_content_negotiation(request)?;
            ReadForWrite::build_response(
                move |w| {
                    Ok((
                        QueryResultsSerializer::from_format(format)
                            .solutions_writer(w, solutions.variables().to_vec())?,
                        solutions,
                    ))
                },
                |(mut writer, mut solutions)| {
                    Ok(if let Some(solution) = solutions.next() {
                        writer.write(&solution?)?;
                        Some((writer, solutions))
                    } else {
                        writer.finish()?;
                        None
                    })
                },
                format.media_type(),
            )
        }
        _ => Err(bad_request("invalid query results")),
    }
}

// parse a zk-SPARQL query
fn parse_zk_query(query: &str, request: &Request) -> Result<ZkQuery, HttpError> {
    let parsed_query = spargebra::Query::parse(query, Some(&base_url(request)))
        .map_err(|e| bad_request(format!("Invalid query: {:?}", e)))?;
    match parsed_query {
        spargebra::Query::Construct { .. } => {
            Err(bad_request("CONSTRUCT is not supported in zk-SPARQL"))
        }
        spargebra::Query::Describe { .. } => {
            Err(bad_request("DESCRIBE is not supported in zk-SPARQL"))
        }
        spargebra::Query::Select {
            dataset,
            pattern,
            base_iri,
        } => parse_zk_select(dataset, pattern, base_iri),
        spargebra::Query::Ask {
            dataset,
            pattern,
            base_iri,
        } => parse_zk_ask(dataset, pattern, base_iri),
    }
}

fn parse_zk_select(
    _dataset: Option<QueryDataset>,
    pattern: GraphPattern,
    _base_iri: Option<Iri<String>>,
) -> Result<ZkQuery, HttpError> {
    println!("original pattern: {:#?}", pattern);

    match pattern {
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => match *inner {
            GraphPattern::Project { inner, variables } => {
                parse_zk_common(*inner, variables, Some(ZkQueryLimit { start, length }))
            }
            _ => Err(bad_request("invalid SELECT query")),
        },
        GraphPattern::Project { inner, variables } => parse_zk_common(*inner, variables, None),
        _ => Err(bad_request("invalid SELECT query")),
    }
}

fn parse_zk_ask(
    _dataset: Option<QueryDataset>,
    pattern: GraphPattern,
    _base_iri: Option<Iri<String>>,
) -> Result<ZkQuery, HttpError> {
    println!("original pattern: {:#?}", pattern);

    match pattern {
        GraphPattern::Slice {
            inner,
            start,
            length,
        } => parse_zk_common(*inner, vec![], Some(ZkQueryLimit { start, length })),
        _ => parse_zk_common(pattern, vec![], None),
    }
}

fn parse_zk_common(
    pattern: GraphPattern,
    disclosed_variables: Vec<Variable>,
    limit: Option<ZkQueryLimit>,
) -> Result<ZkQuery, HttpError> {
    let mut in_scope_variables = HashSet::new();
    pattern.on_in_scope_variable(|v| {
        in_scope_variables.insert(v.clone());
    });
    match pattern {
        GraphPattern::Filter { expr, inner } => match *inner {
            GraphPattern::Bgp { patterns } => Ok(ZkQuery {
                disclosed_variables,
                in_scope_variables,
                patterns,
                filter: Some(expr),
                limit,
                ..Default::default()
            }),
            GraphPattern::Join { left, right } => match (*left, *right) {
                (
                    GraphPattern::Values {
                        variables,
                        bindings,
                    },
                    GraphPattern::Bgp { patterns },
                ) => Ok(ZkQuery {
                    disclosed_variables,
                    in_scope_variables,
                    patterns,
                    filter: Some(expr),
                    values: Some(ZkQueryValues {
                        variables,
                        bindings,
                    }),
                    limit,
                }),
                _ => Err(bad_request("invalid query")),
            },
            _ => Err(bad_request("invalid query")),
        },
        GraphPattern::Bgp { patterns } => Ok(ZkQuery {
            disclosed_variables,
            in_scope_variables,
            patterns,
            limit,
            ..Default::default()
        }),
        GraphPattern::Join { left, right } => match (*left, *right) {
            (
                GraphPattern::Values {
                    variables,
                    bindings,
                },
                GraphPattern::Bgp { patterns },
            ) => Ok(ZkQuery {
                disclosed_variables,
                in_scope_variables,
                patterns,
                values: Some(ZkQueryValues {
                    variables,
                    bindings,
                }),
                limit,
                ..Default::default()
            }),
            _ => Err(bad_request("invalid query")),
        },
        _ => Err(bad_request("invalid query")),
    }
}

// construct an extended query to identify credentials to be disclosed
fn construct_extended_query(query: ZkQuery) -> Result<spargebra::Query, HttpError> {
    // TODO: replace the variable prefix `ggggg` with randomized one
    let extended_graph_variables: Vec<_> = (0..query.patterns.len())
        .map(|i| Variable::new_unchecked(format!("ggggg{}", i)))
        .collect();

    let extended_bgp = query
        .patterns
        .into_iter()
        .enumerate()
        .map(|(i, triple_pattern)| {
            let v = extended_graph_variables
                .get(i)
                .ok_or(bad_request("extended_variables: out of index"))?;
            Ok(GraphPattern::Graph {
                name: NamedNodePattern::Variable(v.clone()),
                inner: Box::new(GraphPattern::Bgp {
                    patterns: vec![triple_pattern],
                }),
            })
        })
        .collect::<Result<Vec<GraphPattern>, _>>()?
        .into_iter()
        .reduce(|left, right| GraphPattern::Join {
            left: Box::new(left),
            right: Box::new(right),
        })
        .unwrap_or_default();

    let extended_bgp_with_values = match query.values {
        Some(ZkQueryValues {
            variables,
            bindings,
        }) => GraphPattern::Join {
            left: Box::new(GraphPattern::Values {
                variables,
                bindings,
            }),
            right: Box::new(extended_bgp),
        },
        _ => extended_bgp,
    };

    let extended_bgp_with_values_and_filter = match query.filter {
        Some(filter) => GraphPattern::Filter {
            expr: filter,
            inner: Box::new(extended_bgp_with_values),
        },
        None => extended_bgp_with_values,
    };

    let extended_graph_pattern = match query.limit {
        Some(limit) => GraphPattern::Slice {
            inner: Box::new(extended_bgp_with_values_and_filter),
            start: limit.start,
            length: limit.length,
        },
        _ => extended_bgp_with_values_and_filter,
    };

    //let mut extended_variables: Vec<_> = query.in_scope_variables.into_iter().collect();
    let mut extended_variables = query.disclosed_variables;
    extended_variables.extend(extended_graph_variables.into_iter());

    Ok(spargebra::Query::Select {
        dataset: None,
        pattern: GraphPattern::Distinct {
            inner: Box::new(GraphPattern::Project {
                inner: Box::new(extended_graph_pattern),
                variables: extended_variables,
            }),
        },
        base_iri: None,
    })
}
